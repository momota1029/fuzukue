use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::ops::Range;

use anyhow::{ensure, Result};
use unsafe_libyaml as yaml;

pub(super) struct Scalar<'a> {
    pub value: &'a str,
    pub anchor: Option<&'a str>,
    pub tag: Option<&'a str>,
    pub plain: bool,
}

pub(super) enum Event<'a> {
    Scalar(Scalar<'a>),
    Alias,
    Mapping { flow: bool },
    Sequence,
    End,
}

// serde_yaml uses the same parser but does not expose scalar styles or byte ranges.
// The parser borrows input. Each event owns its decoded strings through the visit.
pub(super) fn visit(
    input: &str,
    mut visit: impl FnMut(Event<'_>, Range<usize>) -> Result<()>,
) -> Result<()> {
    struct Parser(*mut yaml::yaml_parser_t);
    impl Drop for Parser {
        fn drop(&mut self) {
            unsafe { yaml::yaml_parser_delete(self.0) }
        }
    }

    let mut parser = MaybeUninit::<yaml::yaml_parser_t>::uninit();
    // SAFETY: parser stays at this address until its guard deletes it. Input is
    // valid UTF-8 and lives through the parse. Successful events are deleted
    // once, after all borrows from the event have ended.
    unsafe {
        let ptr = parser.as_mut_ptr();
        ensure!(
            yaml::yaml_parser_initialize(ptr).ok,
            "cannot initialize YAML parser"
        );
        let parser = Parser(ptr);
        yaml::yaml_parser_set_encoding(parser.0, yaml::YAML_UTF8_ENCODING);
        yaml::yaml_parser_set_input_string(parser.0, input.as_ptr(), input.len() as u64);
        loop {
            let mut event = MaybeUninit::<yaml::yaml_event_t>::uninit();
            ensure!(
                yaml::yaml_parser_parse(parser.0, event.as_mut_ptr()).ok,
                "invalid card YAML"
            );
            let mut event = event.assume_init();
            let end = event.type_ == yaml::YAML_STREAM_END_EVENT;
            let span = event.start_mark.index as usize..event.end_mark.index as usize;
            let event_result = match event.type_ {
                yaml::YAML_SCALAR_EVENT => {
                    let scalar = event.data.scalar;
                    let value = std::slice::from_raw_parts(scalar.value, scalar.length as usize);
                    let anchor = (!scalar.anchor.is_null())
                        .then(|| CStr::from_ptr(scalar.anchor.cast()).to_str())
                        .transpose();
                    let tag = (!scalar.tag.is_null())
                        .then(|| CStr::from_ptr(scalar.tag.cast()).to_str())
                        .transpose();
                    match (std::str::from_utf8(value), anchor, tag) {
                        (Ok(value), Ok(anchor), Ok(tag)) => visit(
                            Event::Scalar(Scalar {
                                value,
                                anchor,
                                tag,
                                plain: scalar.plain_implicit,
                            }),
                            span,
                        ),
                        _ => Err(anyhow::anyhow!("invalid UTF-8 in card YAML")),
                    }
                }
                yaml::YAML_ALIAS_EVENT => visit(Event::Alias, span),
                yaml::YAML_MAPPING_START_EVENT => visit(
                    Event::Mapping {
                        flow: event.data.mapping_start.style == yaml::YAML_FLOW_MAPPING_STYLE,
                    },
                    span,
                ),
                yaml::YAML_SEQUENCE_START_EVENT => visit(Event::Sequence, span),
                yaml::YAML_MAPPING_END_EVENT | yaml::YAML_SEQUENCE_END_EVENT => {
                    visit(Event::End, span)
                }
                _ => Ok(()),
            };
            yaml::yaml_event_delete(&mut event);
            event_result?;
            if end {
                return Ok(());
            }
        }
    }
}
