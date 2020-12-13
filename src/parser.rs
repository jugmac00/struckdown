use std::iter;
use std::ops::Range;

use itertools::Either;
use lazy_static::lazy_static;
use pulldown_cmark as cm;
use regex::Regex;

use crate::event::{
    Alignment, AnnotatedEvent, Attrs, CheckboxEvent, CodeBlockEvent, DirectiveEvent, EndTagEvent,
    Event, FootnoteReferenceEvent, FrontMatter, ImageEvent, InlineCodeEvent, InterpretedTextEvent,
    Location, RawHtmlEvent, StartTagEvent, Str, Tag, TextEvent,
};

lazy_static! {
    static ref TEXT_ROLE_RE: Regex = Regex::new(r"\{([^\r\n\}]+)\}$").unwrap();
    static ref DIRECTIVE_RE: Regex = Regex::new(r"^\{([^\r\n\}]+)\}(?:\s+(.*?))?$").unwrap();
    static ref HEADING_ID_RE: Regex = Regex::new(r"\s+\{#([^\r\n\}]+)\}\s*$").unwrap();
    static ref FRONTMATTER_RE: Regex = Regex::new(r"(?sm)^---\s*$(.*?)^---\s*$\r?\n?").unwrap();
}

/// Reads until the end of a tag and read embedded content as raw string.
///
/// We do this because in markdown/cmark the alt text of an image is in fact
/// supporting a lot of markdown syntax but it usually gets rendered out into
/// an alt attribute.  Because of that we normalize this into text during parsing
/// already so that stream processors don't need to deal with this oddity.  Same
/// applies to reading code blocks.
fn read_raw<'a, 'data, I: Iterator<Item = (cm::Event<'data>, Range<usize>)>>(
    iter: &'a mut I,
) -> Str<'data> {
    let mut depth = 1;
    let mut buffer = None;
    let mut last_event = None;

    macro_rules! ensure_buf {
        () => {
            match buffer {
                Some(ref mut buffer) => buffer,
                None => {
                    let mut buf = String::new();
                    if let Some(last_event) = last_event.take() {
                        buf.push_str(&last_event);
                    }
                    buffer = Some(buf);
                    buffer.as_mut().unwrap()
                }
            }
        };
    }

    macro_rules! push {
        ($expr:expr) => {
            if last_event.is_none() && buffer.is_none() {
                last_event = Some($expr);
            } else {
                ensure_buf!().push_str(&$expr);
            }
        };
    }

    while let Some((event, _)) = iter.next() {
        match event {
            cm::Event::Start(..) => depth += 1,
            cm::Event::End(..) => depth -= 1,
            cm::Event::Text(text) => push!(text),
            cm::Event::Code(code) => push!(code),
            cm::Event::SoftBreak | cm::Event::HardBreak => ensure_buf!().push('\n'),
            _ => {}
        }
        if depth == 0 {
            break;
        }
    }

    match (buffer, last_event) {
        (Some(buf), _) => buf.into(),
        (None, Some(event)) => Str::from_cm_str(event),
        (None, None) => "".into(),
    }
}

/// parse front matter in some text
pub fn split_and_parse_front_matter<'data>(
    source: Str<'data>,
) -> (Option<FrontMatter>, Str<'data>) {
    if let Some(m) = FRONTMATTER_RE.captures(source.as_str()) {
        let g0 = m.get(0).unwrap();
        if let Ok(front_matter) = serde_yaml::from_str(&m[1]) {
            return (
                Some(front_matter),
                source.slice(g0.end(), source.as_str().len()),
            );
        }
    }

    (None, source)
}

/// A trailer is information that gets attached to the start tag when the end
/// tag is emitted.
///
/// Trailers are supported internally on all tags for which [`tag_supports_trailers`]
/// returns `true`.
enum Trailer<'data> {
    /// Defines the id attribute via trailer.
    Id(Str<'data>),
}

/// Checks if a tag supports trailers.
///
/// Currently all headlines are the only tags supporting trailers.
fn tag_supports_trailers(tag: Tag) -> bool {
    match tag {
        Tag::Heading1 => true,
        Tag::Heading2 => true,
        Tag::Heading3 => true,
        Tag::Heading4 => true,
        Tag::Heading5 => true,
        Tag::Heading6 => true,
        _ => false,
    }
}
// helper for table state
pub struct TableState {
    alignments: Vec<Alignment>,
    cell_is_head: bool,
    cell_index: usize,
}

/// Parses a string into an event stream with trailers.
///
/// This is normally not used as the `parse` function will already buffer
/// as necessary to automatically attach the trailers to the start tags.
///
/// The output of this parsing function still largely reflects the cmark
/// stream in structure though some elements are already resolved.  The
/// main parse function however will attach some virtual elements such as
/// table bodies which are not there in regular cmark.
fn preliminary_parse_with_trailers<'data>(
    s: &'data str,
) -> impl Iterator<Item = (AnnotatedEvent, Option<Trailer<'data>>)> {
    let mut opts = cm::Options::empty();
    opts.insert(cm::Options::ENABLE_TABLES);
    opts.insert(cm::Options::ENABLE_STRIKETHROUGH);
    opts.insert(cm::Options::ENABLE_TASKLISTS);
    opts.insert(cm::Options::ENABLE_FOOTNOTES);

    let parser = cm::Parser::new_with_broken_link_callback(s, opts, None);
    let mut iter = parser.into_offset_iter().peekable();
    let mut tag_stack = vec![];
    let mut pending_role = None;
    let mut pending_trailer = None;
    let mut table_state = None;

    iter::from_fn(move || {
        let mut trailer = None;

        if let Some((event, range)) = iter.next() {
            // inefficient way to find the location
            let mut loc = Location {
                offset: range.start,
                len: range.end - range.start,
                line: s[..range.start].chars().filter(|&c| c == '\n').count() + 1,
                column: match s[..range.start].rfind('\n') {
                    Some(nl) => range.start - nl - 1,
                    None => range.start,
                },
            };

            // simple events
            let ty = match event {
                cm::Event::Start(cm_tag) => {
                    let mut attrs = Attrs::default();
                    let tag = match cm_tag {
                        cm::Tag::Paragraph => Tag::Paragraph,
                        cm::Tag::Heading(1) => Tag::Heading1,
                        cm::Tag::Heading(2) => Tag::Heading2,
                        cm::Tag::Heading(3) => Tag::Heading3,
                        cm::Tag::Heading(4) => Tag::Heading4,
                        cm::Tag::Heading(5) => Tag::Heading5,
                        cm::Tag::Heading(6) => Tag::Heading6,
                        cm::Tag::Heading(_) => unreachable!(),
                        cm::Tag::BlockQuote => Tag::BlockQuote,
                        cm::Tag::CodeBlock(kind) => match kind {
                            cm::CodeBlockKind::Fenced(lang) => {
                                let lang = Str::from_cm_str(lang);
                                if let Some(m) = DIRECTIVE_RE.captures(lang.as_str()) {
                                    let g1 = m.get(1).unwrap();
                                    let arg = if let Some(g2) = m.get(2) {
                                        lang.slice(g2.start(), g2.end())
                                    } else {
                                        "".into()
                                    };
                                    let body = read_raw(&mut iter);
                                    let (front_matter, body) = split_and_parse_front_matter(body);
                                    return Some((
                                        AnnotatedEvent::new_with_location(
                                            Event::Directive(DirectiveEvent {
                                                name: lang.slice(g1.start(), g1.end()),
                                                argument: if arg.as_str().is_empty() {
                                                    None
                                                } else {
                                                    Some(arg)
                                                },
                                                front_matter,
                                                body,
                                            }),
                                            loc,
                                        ),
                                        None,
                                    ));
                                } else {
                                    let code = read_raw(&mut iter);
                                    return Some((
                                        AnnotatedEvent::new_with_location(
                                            Event::CodeBlock(CodeBlockEvent {
                                                language: Some(lang),
                                                code,
                                            }),
                                            loc,
                                        ),
                                        None,
                                    ));
                                }
                            }
                            cm::CodeBlockKind::Indented => {
                                let code = read_raw(&mut iter);
                                return Some((
                                    AnnotatedEvent::new_with_location(
                                        Event::CodeBlock(CodeBlockEvent {
                                            language: None,
                                            code,
                                        }),
                                        loc,
                                    ),
                                    None,
                                ));
                            }
                        },
                        cm::Tag::List(None) => Tag::UnorderedList,
                        cm::Tag::List(Some(start)) => {
                            attrs.start = Some(start as u32);
                            Tag::OrderedList
                        }
                        cm::Tag::Item => Tag::ListItem,
                        cm::Tag::FootnoteDefinition(id) => {
                            attrs.id = Some(Str::from_cm_str(id));
                            Tag::FootnoteDefinition
                        }
                        cm::Tag::Table(alignments) => {
                            table_state = Some(TableState {
                                alignments: alignments
                                    .into_iter()
                                    .map(|cm_align| match cm_align {
                                        cm::Alignment::None => Alignment::None,
                                        cm::Alignment::Left => Alignment::Left,
                                        cm::Alignment::Center => Alignment::Center,
                                        cm::Alignment::Right => Alignment::Right,
                                    })
                                    .collect(),
                                cell_is_head: false,
                                cell_index: 0,
                            });
                            Tag::Table
                        }
                        cm::Tag::TableHead => {
                            let ref mut state = table_state.as_mut().expect("not in table");
                            state.cell_index = 0;
                            state.cell_is_head = true;
                            Tag::TableHeader
                        }
                        cm::Tag::TableRow => {
                            let ref mut state = table_state.as_mut().expect("not in table");
                            state.cell_index = 0;
                            state.cell_is_head = false;
                            Tag::TableRow
                        }
                        cm::Tag::TableCell => {
                            let ref mut state = table_state.as_mut().expect("not in table");
                            attrs.alignment = state
                                .alignments
                                .get(state.cell_index)
                                .copied()
                                .unwrap_or(Alignment::None);
                            state.cell_index += 1;
                            if state.cell_is_head {
                                Tag::TableHead
                            } else {
                                Tag::TableCell
                            }
                        }
                        cm::Tag::Emphasis => Tag::Emphasis,
                        cm::Tag::Strong => Tag::Strong,
                        cm::Tag::Strikethrough => Tag::Strikethrough,
                        cm::Tag::Link(_, target, title) => {
                            attrs.target = Some(Str::from_cm_str(target));
                            if !title.is_empty() {
                                attrs.title = Some(Str::from_cm_str(title));
                            }
                            Tag::Link
                        }
                        cm::Tag::Image(_, target, title) => {
                            // images are special in that we downgrade them from
                            // tags to toplevel events to not have to deal with
                            // nested text.
                            let alt = read_raw(&mut iter);
                            return Some((
                                AnnotatedEvent::new_with_location(
                                    Event::Image(ImageEvent {
                                        target: Str::from_cm_str(target),
                                        alt: if alt.as_str().is_empty() {
                                            None
                                        } else {
                                            Some(alt)
                                        },
                                        title: if title.is_empty() {
                                            None
                                        } else {
                                            Some(Str::from_cm_str(title))
                                        },
                                    }),
                                    loc,
                                ),
                                None,
                            ));
                        }
                    };
                    tag_stack.push(tag);
                    Event::StartTag(StartTagEvent { tag, attrs })
                }
                cm::Event::End(_) => {
                    trailer = pending_trailer.take();
                    Event::EndTag(EndTagEvent {
                        tag: tag_stack.pop().unwrap(),
                    })
                }
                cm::Event::Text(text) => {
                    let mut text = Str::from_cm_str(text);

                    // handle roles
                    if let Some(&(cm::Event::Code(_), _)) = iter.peek() {
                        if let Some(m) = TEXT_ROLE_RE.captures(text.as_str()) {
                            let g0 = m.get(0).unwrap();
                            let g1 = m.get(1).unwrap();

                            // adjust the span of the text to not include the role.
                            let column_adjustment = g0.end() - g0.start();
                            loc.len -= column_adjustment;
                            pending_role =
                                Some((text.slice(g1.start(), g1.end()), column_adjustment));
                            text = text.slice(0, g0.start());
                        }
                    }

                    // handle explicitly defined IDs for headlines
                    if let Some(&(cm::Event::End(cm::Tag::Heading(_)), _)) = iter.peek() {
                        if let Some(m) = HEADING_ID_RE.captures(text.as_str()) {
                            let g0 = m.get(0).unwrap();
                            let g1 = m.get(1).unwrap();

                            // adjust the span of the text to not include the role.
                            let column_adjustment = g0.end() - g0.start();
                            loc.len -= column_adjustment;
                            pending_trailer = Some(Trailer::Id(text.slice(g1.start(), g1.end())));
                            text = text.slice(0, g0.start());
                        }
                    }

                    Event::Text(TextEvent { text })
                }
                cm::Event::Code(value) => {
                    // if there is a pending role then we're not working with a
                    // code block, but an interpreted text one.
                    if let Some((role, column_adjustment)) = pending_role.take() {
                        loc.column -= column_adjustment;
                        loc.offset -= column_adjustment;
                        loc.len += column_adjustment;
                        Event::InterpretedText(InterpretedTextEvent {
                            text: Str::from_cm_str(value),
                            role: role.into(),
                        })
                    } else {
                        Event::InlineCode(InlineCodeEvent {
                            code: Str::from_cm_str(value),
                        })
                    }
                }
                cm::Event::Html(html) => Event::RawHtml(RawHtmlEvent {
                    html: Str::from_cm_str(html),
                }),
                cm::Event::FootnoteReference(target) => {
                    Event::FootnoteReference(FootnoteReferenceEvent {
                        target: Str::from_cm_str(target),
                    })
                }
                cm::Event::SoftBreak => Event::SoftBreak,
                cm::Event::HardBreak => Event::HardBreak,
                cm::Event::Rule => Event::Rule,
                cm::Event::TaskListMarker(checked) => Event::Checkbox(CheckboxEvent { checked }),
            };

            Some((AnnotatedEvent::new_with_location(ty, loc), trailer))
        } else {
            None
        }
    })
}

/// Recursively attaches trailers to start tags.
fn buffer_for_trailers<'data, I>(
    event: AnnotatedEvent<'data>,
    iter: &mut I,
) -> Vec<AnnotatedEvent<'data>>
where
    I: Iterator<Item = (AnnotatedEvent<'data>, Option<Trailer<'data>>)>,
{
    let mut buffer = vec![event];
    let mut depth = 1;

    while let Some((event, trailer)) = iter.next() {
        // keep track of the tag depth
        match event.event() {
            &Event::StartTag(StartTagEvent { tag, .. }) => {
                if tag_supports_trailers(tag) {
                    buffer.extend(buffer_for_trailers(event, iter));
                    continue;
                } else {
                    depth += 1;
                }
            }
            &Event::EndTag { .. } => depth -= 1,
            _ => {}
        }
        buffer.push(event);

        // attach an end tag trailer to the start tag if needed.
        if depth == 0 {
            if let Event::StartTag(StartTagEvent { attrs, .. }) = buffer[0].event_mut() {
                match trailer {
                    Some(Trailer::Id(new_id)) => {
                        attrs.id = Some(new_id);
                    }
                    None => {}
                }
            }
            break;
        }
    }

    buffer
}

/// Parses structured cmark into an event stream.
pub fn parse(s: &str) -> impl Iterator<Item = AnnotatedEvent> {
    let mut iter = preliminary_parse_with_trailers(s);

    iter::from_fn(move || {
        if let Some((event, _)) = iter.next() {
            if let &Event::StartTag(StartTagEvent { tag, .. }) = event.event() {
                if tag_supports_trailers(tag) {
                    return Some(Either::Left(
                        buffer_for_trailers(event, &mut iter).into_iter(),
                    ));
                }
            }
            Some(Either::Right(iter::once(event)))
        } else {
            None
        }
    })
    .flatten()
    .flat_map(|event| match event.event() {
        // after a table header we inject an implied table body.
        Event::EndTag(EndTagEvent {
            tag: Tag::TableHeader,
        }) => Either::Left(
            iter::once(event).chain(iter::once(
                Event::StartTag(StartTagEvent {
                    tag: Tag::TableBody,
                    attrs: Default::default(),
                })
                .into(),
            )),
        ),
        // just before the table end, we close the table body.
        Event::EndTag(EndTagEvent { tag: Tag::Table }) => Either::Left(
            iter::once(
                Event::EndTag(EndTagEvent {
                    tag: Tag::TableBody,
                })
                .into(),
            )
            .chain(iter::once(event)),
        ),
        _ => Either::Right(iter::once(event)),
    })
}
