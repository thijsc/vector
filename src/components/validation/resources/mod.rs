mod event;
mod http;

use codecs::{
    decoding::{self, DeserializerConfig},
    encoding::{self, Framer, FramingConfig, SerializerConfig},
    BytesEncoder,
};
use tokio::sync::mpsc;
use vector_core::{config::DataType, event::Event};

use crate::codecs::{DecodingConfig, Encoder, EncodingConfig, EncodingConfigWithFraming};

pub use self::event::TestEvent;
pub use self::http::HttpConfig;

use super::sync::{Configuring, TaskCoordinator};

/// The codec used by the external resource.
///
/// This enum specifically exists to encapsulate the two main ways a component will configure the
/// codec it uses, which ends up being with directionally-specific codec configuration: "encoding"
/// when taking an `Event` and convert it to a raw output, and "decoding" when taking a raw output
/// and converting it to an `Event`.
///
/// Encoding and decoding is generally tied to sinks and sources, respectively.
#[derive(Clone)]
pub enum ResourceCodec {
    /// Component encodes events.
    ///
    /// As opposed to `EncodingWithFramer`, this variant uses the default framing method defined by
    /// the encoding itself.
    ///
    /// Generally speaking, only sinks encode: going from `Event` to an encoded form.
    Encoding(EncodingConfig),

    /// Component encodes events, with a specific framer.
    ///
    /// Generally speaking, only sinks encode: going from `Event` to an encoded form.
    EncodingWithFraming(EncodingConfigWithFraming),

    /// Component decodes events.
    ///
    /// As opposed to `DecodingWithFramer`, this variant uses the default framing method defined by
    /// the decoding itself.
    ///
    /// Generally speaking, only sources decode: going from an encoded form to `Event`.
    Decoding(DecodingConfig),

    /// Component decodes events, with a specific framer.
    ///
    /// Generally speaking, only sources decode: going from an encoded form to `Event`.
    DecodingWithFraming(DecodingConfig, decoding::FramingConfig),
}

impl ResourceCodec {
    /// Gets the allowed event data types for the configured codec.
    ///
    /// Not all codecs support all possible event types (i.e. a codec has no means to losslessly
    /// represent the data in a particular event type) so we must check at runtime to ensure that
    /// we're only generating event payloads that can be encoded/decoded for the given component.
    pub fn allowed_event_data_types(self) -> DataType {
        match self {
            Self::Encoding(encoding) => encoding.config().input_type(),
            Self::EncodingWithFraming(encoding) => encoding.config().1.input_type(),
            Self::Decoding(decoding) | Self::DecodingWithFraming(decoding, _) => {
                decoding.config().output_type()
            }
        }
    }

    /// Gets an encoder for this codec.
    ///
    /// The encoder is generated as an inverse to the input codec: if a decoding configuration was
    /// given, we generate an encoder that satisfies that decoding configuration, and vise versa.
    pub fn into_encoder(&self) -> Encoder<encoding::Framer> {
        let (framer, serializer) = match self {
            Self::Encoding(config) => (
                Framer::Bytes(BytesEncoder::new()),
                config.build().expect("should not fail to build serializer"),
            ),
            Self::EncodingWithFraming(config) => {
                let (maybe_framing, serializer) = config.config();
                (
                    maybe_framing
                        .clone()
                        .unwrap_or(FramingConfig::Bytes)
                        .build(),
                    serializer
                        .build()
                        .expect("building serializer should never fail"),
                )
            }
            Self::Decoding(config) => (
                decoder_framing_to_encoding_framer(&config.config().default_stream_framing()),
                deserializer_config_to_serializer(config.config()),
            ),
            Self::DecodingWithFraming(config, framing) => (
                decoder_framing_to_encoding_framer(framing),
                deserializer_config_to_serializer(config.config()),
            ),
        };

        Encoder::<encoding::Framer>::new(framer, serializer)
    }
}

impl From<EncodingConfig> for ResourceCodec {
    fn from(config: EncodingConfig) -> Self {
        Self::Encoding(config)
    }
}

impl From<EncodingConfigWithFraming> for ResourceCodec {
    fn from(config: EncodingConfigWithFraming) -> Self {
        Self::EncodingWithFraming(config)
    }
}

impl From<DecodingConfig> for ResourceCodec {
    fn from(config: DecodingConfig) -> Self {
        Self::Decoding(config)
    }
}

fn deserializer_config_to_serializer(config: &DeserializerConfig) -> encoding::Serializer {
    let serializer_config = match config {
        // TODO: This isn't necessarily a one-to-one conversion, at least not in the future when
        // "bytes" can be a top-level field and we aren't implicitly decoding everything into the
        // `message` field... but it's close enough for now.
        DeserializerConfig::Bytes => SerializerConfig::Text,
        DeserializerConfig::Json => SerializerConfig::Json,
        // TODO: We need to create an Avro serializer because, certainly, for any source decoding
        // the data as Avro, we can't possibly send anything else without the source just
        // immediately barfing.
        #[cfg(feature = "sources-syslog")]
        DeserializerConfig::Syslog => SerializerConfig::Logfmt,
        DeserializerConfig::Native => SerializerConfig::Native,
        DeserializerConfig::NativeJson => SerializerConfig::NativeJson,
        DeserializerConfig::Gelf => SerializerConfig::Gelf,
    };

    serializer_config
        .build()
        .expect("building serializer should never fail")
}

fn decoder_framing_to_encoding_framer(framing: &decoding::FramingConfig) -> encoding::Framer {
    let framing_config = match framing {
        decoding::FramingConfig::Bytes => encoding::FramingConfig::Bytes,
        decoding::FramingConfig::CharacterDelimited {
            character_delimited,
        } => encoding::FramingConfig::CharacterDelimited {
            character_delimited: encoding::CharacterDelimitedEncoderOptions {
                delimiter: character_delimited.delimiter,
            },
        },
        decoding::FramingConfig::LengthDelimited => encoding::FramingConfig::LengthDelimited,
        decoding::FramingConfig::NewlineDelimited { .. } => {
            encoding::FramingConfig::NewlineDelimited
        }
        // TODO: There's no equivalent octet counting framer for encoding... although
        // there's no particular reason that would make it hard to write.
        decoding::FramingConfig::OctetCounting { .. } => todo!(),
    };

    framing_config.build()
}

/// Direction that the resource is operating in.
pub enum ResourceDirection {
    /// Resource will have the component pull data from it, or pull data from the component.
    ///
    /// For a source, where an external resource functions in "input" mode, this would be the
    /// equivalent of the source calling out to the external resource (HTTP server, Kafka cluster,
    /// etc) and asking for data, or expecting it to be returned in the response.
    ///
    /// For a sink, where an external resource functions in "output" mode, this would be the
    /// equivalent of the sink exposing a network endpoint and having the external resource be
    /// responsible for connecting to the endpoint to grab the data.
    Pull,

    /// Resource will push data to the component, or have data pushed to it from the component.
    ///
    /// For a source, where an external resource functions in "input" mode, this would be the
    /// equivalent of the source waiting for data to be sent to either, whether it's listening on a
    /// network endpoint for traffic, or polling files on disks for updates, and the external
    /// resource would be responsible for initiating that communication, or writing to those files.
    ///
    /// For a sink, where an external resource functions in "output" mode, this would be the
    /// equivalent of the sink pushing its data to a network endpoint, or writing data to files,
    /// where the external resource would be responsible for aggregating that data, or read from
    /// those files.
    Push,
}

/// A resource definition.
///
/// Resource definitions uniquely identify the resource, such as HTTP, or files, and so on. These
/// definitions generally include the bare minimum amount of information to allow the component
/// validation runner to create an instance of them, such as spawning an HTTP server if a source has
/// specified an HTTP resource in the "pull" direction.
pub enum ResourceDefinition {
    Http(HttpConfig),
}

impl From<HttpConfig> for ResourceDefinition {
    fn from(config: HttpConfig) -> Self {
        Self::Http(config)
    }
}

/// An external resource associated with a component.
///
/// External resources represent the hypothetical location where, depending on whether the component
/// is a source or sink, data would be generated from or collected at. This includes things like
/// network endpoints (raw sockets, HTTP servers, etc) as well as files on disk, and more. In other
/// words, an external resource is a data dependency associated with the component, whether the
/// component depends on data from the external resource, or the external resource depends on data
/// from the component.
///
/// An external resource includes a direction -- push or pull -- as well as the fundamental
/// definition of the resource, such as HTTP or file. The component type is used to further refine
/// the direction of the resource, such that a "pull" resource used with a source implies the source
/// will pull data from the external resource, whereas a "pull" resource used with a sink implies
/// the external resource must pull the data from the sink.
pub struct ExternalResource {
    direction: ResourceDirection,
    definition: ResourceDefinition,
    codec: ResourceCodec,
}

impl ExternalResource {
    /// Creates a new `ExternalResource` based on the given `direction`, `definition`, and `codec`.
    pub fn new<D, C>(direction: ResourceDirection, definition: D, codec: C) -> Self
    where
        D: Into<ResourceDefinition>,
        C: Into<ResourceCodec>,
    {
        Self {
            direction,
            definition: definition.into(),
            codec: codec.into(),
        }
    }

    /// Spawns this resource for use as an input to a source.
    pub fn spawn_as_input(
        self,
        input_rx: mpsc::Receiver<TestEvent>,
        task_coordinator: &TaskCoordinator<Configuring>,
    ) {
        match self.definition {
            ResourceDefinition::Http(http_config) => {
                http_config.spawn_as_input(self.direction, self.codec, input_rx, task_coordinator)
            }
        }
    }

    /// Spawns this resource for use as an output for a sink.
    pub fn spawn_as_output(
        self,
        output_tx: mpsc::Sender<Event>,
        task_coordinator: &TaskCoordinator<Configuring>,
    ) {
        match self.definition {
            ResourceDefinition::Http(http_config) => {
                http_config.spawn_as_output(self.direction, self.codec, output_tx, task_coordinator)
            }
        }
    }
}
