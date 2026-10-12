//! A minimal, namespace-agnostic XML tree.
//!
//! eSCL documents mix the `scan:` and `pwg:` namespaces, and vendors are not
//! consistent about which prefix they bind to which URI. Matching on local
//! names only is what other eSCL implementations do in practice, and it keeps
//! the capability parser simple.

use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

/// An XML element with its local name, attributes, children and text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub children: Vec<Element>,
    /// Concatenated character data, untrimmed.
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum XmlError {
    #[error("malformed XML: {0}")]
    Syntax(#[from] quick_xml::Error),
    #[error("malformed XML attribute: {0}")]
    Attribute(#[from] quick_xml::events::attributes::AttrError),
    #[error("unknown XML entity &{0};")]
    UnknownEntity(String),
    #[error("XML document has no root element")]
    Empty,
}

impl Element {
    /// Parses a whole document and returns its root element.
    pub fn parse(input: &str) -> Result<Self, XmlError> {
        // Text is kept verbatim: trimming here would also eat the spaces
        // around entity references, which arrive as separate events.
        let mut reader = Reader::from_str(input);

        // The bottom of the stack is a synthetic container for the root.
        let mut stack = vec![Element::default()];
        loop {
            match reader.read_event()? {
                Event::Start(start) => stack.push(Element::open(&start)?),
                Event::Empty(start) => {
                    let element = Element::open(&start)?;
                    top(&mut stack).children.push(element);
                }
                Event::End(_) => {
                    // The reader already checks that end tags match, so the
                    // stack always holds at least the container here.
                    if stack.len() > 1 {
                        let element = stack.pop().expect("stack is not empty");
                        top(&mut stack).children.push(element);
                    }
                }
                Event::Text(text) => top(&mut stack).text.push_str(&text.xml10_content()),
                Event::CData(data) => top(&mut stack)
                    .text
                    .push_str(&data.xml_content(XmlVersion::Implicit1_0)),
                Event::GeneralRef(reference) => {
                    let resolved = match reference.resolve_char_ref()? {
                        Some(ch) => ch.to_string(),
                        None => {
                            let name = reference.xml10_content();
                            resolve_predefined_entity(&name)
                                .ok_or_else(|| XmlError::UnknownEntity(name.into_owned()))?
                                .to_owned()
                        }
                    };
                    top(&mut stack).text.push_str(&resolved);
                }
                Event::Eof => break,
                _ => {}
            }
        }
        stack
            .pop()
            .and_then(|container| container.children.into_iter().next())
            .ok_or(XmlError::Empty)
    }

    fn open(start: &BytesStart<'_>) -> Result<Self, XmlError> {
        let name = start.local_name().as_ref().to_owned();
        let attributes = start
            .attributes()
            .map(|attr| {
                let attr = attr?;
                let key = attr.key.local_name().as_ref().to_owned();
                let value = attr.normalized_value(XmlVersion::Implicit1_0)?.into_owned();
                Ok((key, value))
            })
            .collect::<Result<_, XmlError>>()?;
        Ok(Element {
            name,
            attributes,
            ..Default::default()
        })
    }

    /// The first direct child with the given local name.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    /// All direct children with the given local name.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Follows a path of local names through first-matching children.
    pub fn path(&self, path: &[&str]) -> Option<&Element> {
        path.iter()
            .try_fold(self, |element, name| element.child(name))
    }

    /// The trimmed text of the first child with the given local name.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }

    /// The text of the first child with the given name, parsed as a number.
    pub fn child_parse<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.child_text(name)?.parse().ok()
    }

    /// The value of an attribute, matched by local name.
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn top(stack: &mut [Element]) -> &mut Element {
    stack.last_mut().expect("stack always holds the container")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_namespaced_document_by_local_name() {
        let doc = r#"<?xml version="1.0"?>
            <scan:Root xmlns:scan="urn:a" xmlns:pwg="urn:b">
              <pwg:Version>2.63</pwg:Version>
              <scan:List><scan:Item scan:default="true">a</scan:Item><scan:Item>b</scan:Item></scan:List>
              <scan:Empty/>
            </scan:Root>"#;
        let root = Element::parse(doc).unwrap();
        assert_eq!(root.name, "Root");
        assert_eq!(root.child_text("Version"), Some("2.63"));
        assert_eq!(root.child_parse::<f32>("Version"), Some(2.63));
        let items: Vec<_> = root.child("List").unwrap().children_named("Item").collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].attribute("default"), Some("true"));
        assert_eq!(items[1].text, "b");
        assert!(root.child("Empty").is_some());
        assert!(root.path(&["List", "Item"]).is_some());
    }

    #[test]
    fn resolves_entities_and_cdata() {
        let root = Element::parse("<a>x &amp; y &#x41;<![CDATA[<z>]]></a>").unwrap();
        assert_eq!(root.text, "x & y A<z>");
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(Element::parse("<a><b></a>").is_err());
        assert!(matches!(Element::parse(""), Err(XmlError::Empty)));
    }
}
