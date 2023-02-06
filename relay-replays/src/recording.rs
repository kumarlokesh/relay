use std::borrow::Cow;
use std::fmt;
use std::io::{BufReader, Read};

use flate2::{bufread::ZlibDecoder, write::ZlibEncoder, Compression};
use once_cell::sync::Lazy;

use relay_general::pii::{PiiConfig, PiiProcessor};
use relay_general::processor::{FieldAttrs, Pii, ProcessingState, Processor, ValueType};
use relay_general::types::Meta;

use crate::transform::{self, Transform};

#[derive(Debug)]
pub enum ParseRecordingError {
    Json(serde_json::Error),
    Message(&'static str),
}

impl fmt::Display for ParseRecordingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseRecordingError::Json(serde_error) => write!(f, "{serde_error}"),
            ParseRecordingError::Message(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ParseRecordingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ParseRecordingError::Json(e) => Some(e),
            ParseRecordingError::Message(_) => None,
        }
    }
}

impl From<serde_json::Error> for ParseRecordingError {
    fn from(err: serde_json::Error) -> Self {
        ParseRecordingError::Json(err)
    }
}

static STRING_STATE: Lazy<ProcessingState> = Lazy::new(|| {
    ProcessingState::root().enter_static(
        "",
        Some(Cow::Owned(FieldAttrs::new().pii(Pii::True))),
        Some(ValueType::String),
    )
});

/// A utility that performs data scrubbing on Replay payloads.
pub struct ReplayScrubber<'a> {
    limit: usize,
    processor1: Option<PiiProcessor<'a>>,
    processor2: Option<PiiProcessor<'a>>,
}

impl<'a> ReplayScrubber<'a> {
    /// Creates a new `ReplayScrubber` from PII configs.
    ///
    /// `limit` controls the maximum size in bytes during decompression. This function returns an
    /// `Err` if decompressed contents exceed the limit. The two optional configs to be passed here
    /// are from data scrubbing settings and from the dedicated PII config.
    pub fn new(
        limit: usize,
        config1: Option<&'a PiiConfig>,
        config2: Option<&'a PiiConfig>,
    ) -> Self {
        Self {
            limit,
            processor1: config1.map(|c| PiiProcessor::new(c.compiled())),
            processor2: config2.map(|c| PiiProcessor::new(c.compiled())),
        }
    }

    /// Returns `true` if both configs are empty and no scrubbing would occur.
    pub fn is_empty(&self) -> bool {
        self.processor1.is_none() && self.processor2.is_none()
    }

    fn scrub_replay<R, W>(&mut self, read: R, write: W) -> Result<(), ParseRecordingError>
    where
        R: std::io::Read,
        W: std::io::Write,
    {
        let mut deserializer = serde_json::Deserializer::from_reader(read);
        let mut serializer = serde_json::Serializer::new(write);

        let transformer = transform::Deserializer::new(&mut deserializer, self);
        serde_transcode::transcode(transformer, &mut serializer)?;

        Ok(())
    }

    #[doc(hidden)] // Public for benchmarks.
    pub fn transcode_replay(
        &mut self,
        body: &[u8],
        output: &mut Vec<u8>,
    ) -> Result<(), ParseRecordingError> {
        let encoder = ZlibEncoder::new(output, Compression::default());

        if body.first() == Some(&b'[') {
            self.scrub_replay(body, encoder)
        } else {
            let decoder = ZlibDecoder::new(body).take(self.limit as u64);
            self.scrub_replay(BufReader::new(decoder), encoder)
        }
    }

    /// Parses compressed replay recording payloads and applies data scrubbers.
    ///
    /// To avoid redundant parsing, check [`is_empty`](Self::is_empty) first.
    pub fn process_recording(&mut self, bytes: &[u8]) -> Result<Vec<u8>, ParseRecordingError> {
        // Check for null byte condition.
        if bytes.is_empty() {
            return Err(ParseRecordingError::Message("no data found"));
        }

        let mut split = bytes.splitn(2, |b| b == &b'\n');
        let header = split
            .next()
            .ok_or(ParseRecordingError::Message("no headers found"))?;

        let body = match split.next() {
            Some(b"") | None => return Err(ParseRecordingError::Message("no body found")),
            Some(body) => body,
        };

        let mut output = header.to_owned();
        output.push(b'\n');
        // Data scrubbing usually does not change the size of the output by much. We can preallocate
        // enough space for the scrubbed output to avoid resizing the output buffer serveral times.
        // Benchmarks have NOT shown a big difference, however.
        output.reserve(body.len());
        self.transcode_replay(body, &mut output)?;

        Ok(output)
    }
}

impl Transform for &'_ mut ReplayScrubber<'_> {
    fn transform_str<'a>(&mut self, v: &'a str) -> Cow<'a, str> {
        self.transform_string(v.to_owned())
    }

    fn transform_string(&mut self, mut value: String) -> Cow<'static, str> {
        if let Some(ref mut processor) = self.processor1 {
            if processor
                .process_string(&mut value, &mut Meta::default(), &STRING_STATE)
                .is_err()
            {
                return Cow::Borrowed("");
            }
        }

        if let Some(ref mut processor) = self.processor2 {
            if processor
                .process_string(&mut value, &mut Meta::default(), &STRING_STATE)
                .is_err()
            {
                return Cow::Borrowed("");
            }
        }

        Cow::Owned(value)
    }
}

#[cfg(test)]
mod tests {
    // End to end test coverage.

    use relay_general::pii::{DataScrubbingConfig, PiiConfig};

    use super::ReplayScrubber;

    fn default_pii_config() -> PiiConfig {
        let mut scrubbing_config = DataScrubbingConfig::default();
        scrubbing_config.scrub_data = true;
        scrubbing_config.scrub_defaults = true;
        scrubbing_config.scrub_ip_addresses = true;
        scrubbing_config.pii_config_uncached().unwrap().unwrap()
    }

    fn scrubber(config: &PiiConfig) -> ReplayScrubber {
        ReplayScrubber::new(usize::MAX, Some(config), None)
    }

    #[test]
    fn test_process_recording_end_to_end() {
        // Valid compressed rrweb payload.  Contains a 16 byte header followed by a new line
        // character and concludes with a gzipped rrweb payload.
        let payload: &[u8] = &[
            123, 34, 115, 101, 103, 109, 101, 110, 116, 95, 105, 100, 34, 58, 51, 125, 10, 120,
            156, 149, 144, 91, 106, 196, 32, 20, 64, 247, 114, 191, 237, 160, 241, 145, 234, 38,
            102, 1, 195, 124, 152, 104, 6, 33, 169, 193, 40, 52, 4, 247, 94, 91, 103, 40, 20, 108,
            59, 191, 247, 30, 207, 225, 122, 57, 32, 238, 171, 5, 69, 17, 24, 29, 53, 168, 3, 54,
            159, 194, 88, 70, 4, 193, 234, 55, 23, 157, 127, 219, 64, 93, 14, 120, 7, 37, 100, 1,
            119, 80, 29, 102, 8, 156, 1, 213, 11, 4, 209, 45, 246, 60, 77, 155, 141, 160, 94, 232,
            43, 206, 232, 206, 118, 127, 176, 132, 177, 7, 203, 42, 75, 36, 175, 44, 231, 63, 88,
            217, 229, 107, 174, 179, 45, 234, 101, 45, 172, 232, 49, 163, 84, 22, 191, 232, 63, 61,
            207, 93, 130, 229, 189, 216, 53, 138, 84, 182, 139, 178, 199, 191, 22, 139, 179, 238,
            196, 227, 244, 134, 137, 240, 158, 60, 101, 34, 255, 18, 241, 6, 116, 42, 212, 119, 35,
            234, 27, 40, 24, 130, 213, 102, 12, 105, 25, 160, 252, 147, 222, 103, 175, 205, 215,
            182, 45, 168, 17, 48, 118, 210, 105, 142, 229, 217, 168, 163, 189, 249, 80, 254, 19,
            146, 59, 13, 115, 10, 144, 115, 190, 126, 0, 2, 68, 180, 16,
        ];

        let config = default_pii_config();
        let result = scrubber(&config).process_recording(payload);
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn test_process_recording_no_body_data() {
        // Empty bodies can not be decompressed and fail.
        let payload: &[u8] = &[
            123, 34, 115, 101, 103, 109, 101, 110, 116, 95, 105, 100, 34, 58, 51, 125, 10,
        ];

        let config = default_pii_config();
        let result = scrubber(&config).process_recording(payload);
        assert!(matches!(
            result.unwrap_err(),
            super::ParseRecordingError::Message("no body found"),
        ));
    }

    #[test]
    fn test_process_recording_bad_body_data() {
        // Invalid gzip body contents.  Can not deflate.
        let payload: &[u8] = &[
            123, 34, 115, 101, 103, 109, 101, 110, 116, 95, 105, 100, 34, 58, 51, 125, 10, 22,
        ];

        let config = default_pii_config();
        let result = scrubber(&config).process_recording(payload);
        assert!(matches!(
            result.unwrap_err(),
            super::ParseRecordingError::Json(_),
        ));
    }

    #[test]
    fn test_process_recording_no_headers() {
        // No header delimiter.  Entire payload is consumed as headers.  The empty body fails.
        let payload: &[u8] = &[
            123, 34, 115, 101, 103, 109, 101, 110, 116, 95, 105, 100, 34, 58, 51, 125,
        ];

        let config = default_pii_config();
        let result = scrubber(&config).process_recording(payload);
        assert!(matches!(
            result.unwrap_err(),
            super::ParseRecordingError::Message("no body found"),
        ));
    }

    #[test]
    fn test_process_recording_no_contents() {
        // Empty payload can not be decompressed.  Header check never fails.
        let payload: &[u8] = &[];

        let config = default_pii_config();
        let result = scrubber(&config).process_recording(payload);
        assert!(matches!(
            result.unwrap_err(),
            super::ParseRecordingError::Message("no data found"),
        ));
    }

    // RRWeb Payload Coverage

    #[test]
    fn test_pii_credit_card_removal() {
        let payload = include_bytes!("../tests/fixtures/rrweb-pii.json");

        let mut transcoded = Vec::new();
        let config = default_pii_config();
        scrubber(&config)
            .scrub_replay(payload.as_slice(), &mut transcoded)
            .unwrap();

        let parsed = std::str::from_utf8(&transcoded).unwrap();
        assert!(parsed.contains(r#"{"type":3,"textContent":"[Filtered]","id":284}"#));
    }

    #[test]
    fn test_scrub_pii_navigation() {
        let payload = include_bytes!("../tests/fixtures/rrweb-performance-navigation.json");

        let mut transcoded = Vec::new();
        let config = default_pii_config();
        scrubber(&config)
            .scrub_replay(payload.as_slice(), &mut transcoded)
            .unwrap();

        let parsed = std::str::from_utf8(&transcoded).unwrap();
        assert!(parsed.contains("https://sentry.io?credit-card=[Filtered]"));
    }

    #[test]
    fn test_scrub_pii_resource() {
        let payload = include_bytes!("../tests/fixtures/rrweb-performance-resource.json");

        let mut transcoded = Vec::new();
        let config = default_pii_config();
        scrubber(&config)
            .scrub_replay(payload.as_slice(), &mut transcoded)
            .unwrap();

        let parsed = std::str::from_utf8(&transcoded).unwrap();
        assert!(parsed.contains("https://sentry.io?credit-card=[Filtered]"));
    }

    #[test]
    fn test_pii_ip_address_removal() {
        let payload = include_bytes!("../tests/fixtures/rrweb-pii-ip-address.json");

        let mut transcoded = Vec::new();
        let config = default_pii_config();
        scrubber(&config)
            .scrub_replay(payload.as_slice(), &mut transcoded)
            .unwrap();

        let parsed = std::str::from_utf8(&transcoded).unwrap();
        assert!(parsed.contains("\"value\":\"[ip]\"")); // Assert texts were mutated.
        assert!(parsed.contains("\"textContent\":\"[ip]\"")) // Assert text node was mutated.
    }

    // Event Parsing and Scrubbing.

    #[test]
    fn test_scrub_pii_full_snapshot_event() {
        let payload = include_bytes!("../tests/fixtures/rrweb-event-2.json");

        let mut transcoded = Vec::new();
        let config = default_pii_config();
        scrubber(&config)
            .scrub_replay(payload.as_slice(), &mut transcoded)
            .unwrap();

        let scrubbed_result = std::str::from_utf8(&transcoded).unwrap();
        // NOTE: The normalization below was removed
        // assert!(scrubbed_result.contains("\"attributes\":{\"src\":\"#\"}"));
        assert!(scrubbed_result.contains("\"textContent\":\"my ssn is [Filtered]\""));
    }

    #[test]
    fn test_scrub_pii_incremental_snapshot_event() {
        let payload = include_bytes!("../tests/fixtures/rrweb-event-3.json");

        let mut transcoded = Vec::new();
        let config = default_pii_config();
        scrubber(&config)
            .scrub_replay(payload.as_slice(), &mut transcoded)
            .unwrap();

        let scrubbed_result = std::str::from_utf8(&transcoded).unwrap();
        assert!(scrubbed_result.contains("\"textContent\":\"[Filtered]\""));
        assert!(scrubbed_result.contains("\"value\":\"[Filtered]\""));
    }

    #[test]
    fn test_scrub_pii_custom_event() {
        let payload = include_bytes!("../tests/fixtures/rrweb-event-5.json");

        let mut transcoded = Vec::new();
        let config = default_pii_config();
        scrubber(&config)
            .scrub_replay(payload.as_slice(), &mut transcoded)
            .unwrap();

        let scrubbed_result = std::str::from_utf8(&transcoded).unwrap();
        assert!(scrubbed_result.contains("\"description\":\"[Filtered]\""));
        assert!(scrubbed_result.contains("\"description\":\"https://sentry.io?ip-address=[ip]\""));
        // NOTE: default scrubbers do not remove email address
        // assert!(scrubbed_result.contains("\"message\":\"[email]\""));
    }
}