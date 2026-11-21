//! The small part of XML property lists the usbmux protocol uses: dictionaries,
//! arrays, strings, integers and booleans. Input is untrusted (anything on the
//! machine can answer on the service's address), so parsing is bounded in depth
//! and never panics; anything it does not understand is an error.

/// A property-list value. Kinds the protocol never needs are kept as `Other`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Plist {
    Dict(Vec<(String, Plist)>),
    Array(Vec<Plist>),
    String(String),
    Integer(i64),
    Bool(bool),
    Other,
}

impl Plist {
    pub(crate) fn get(&self, key: &str) -> Option<&Plist> {
        match self {
            Plist::Dict(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub(crate) fn string(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Plist::String(text) => Some(text),
            _ => None,
        }
    }
    pub(crate) fn integer(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Plist::Integer(value) => Some(*value),
            _ => None,
        }
    }
    pub(crate) fn array(&self, key: &str) -> Option<&[Plist]> {
        match self.get(key)? {
            Plist::Array(items) => Some(items),
            _ => None,
        }
    }
}

const MAX_DEPTH: usize = 8;

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// A request: a flat dictionary of strings and integers.
pub(crate) enum Field<'a> {
    Text(&'a str),
    Number(i64),
}

pub(crate) fn request(fields: &[(&str, Field)]) -> Vec<u8> {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    for (key, value) in fields {
        xml.push_str(&format!("\t<key>{}</key>\n", escape(key)));
        match value {
            Field::Text(text) => xml.push_str(&format!("\t<string>{}</string>\n", escape(text))),
            Field::Number(number) => xml.push_str(&format!("\t<integer>{number}</integer>\n")),
        }
    }
    xml.push_str("</dict>\n</plist>\n");
    xml.into_bytes()
}

struct Parser<'a> {
    rest: &'a str,
}

impl<'a> Parser<'a> {
    fn skip_space(&mut self) {
        self.rest = self.rest.trim_start();
    }

    /// The next tag's inside (`dict`, `/dict`, `true/`, …), after any space.
    fn tag(&mut self) -> Option<&'a str> {
        self.skip_space();
        let rest = self.rest.strip_prefix('<')?;
        let end = rest.find('>')?;
        self.rest = &rest[end + 1..];
        Some(rest[..end].trim())
    }

    /// Text up to the closing tag `name`.
    fn text(&mut self, name: &str) -> Option<&'a str> {
        let close = format!("</{name}>");
        let end = self.rest.find(&close)?;
        let text = &self.rest[..end];
        self.rest = &self.rest[end + close.len()..];
        Some(text)
    }

    fn value(&mut self, tag: &str, depth: usize) -> Option<Plist> {
        if depth > MAX_DEPTH {
            return None;
        }
        Some(match tag {
            "dict" => {
                let mut entries = Vec::new();
                loop {
                    match self.tag()? {
                        "/dict" => break,
                        "key" => {
                            let key = unescape(self.text("key")?);
                            let tag = self.tag()?;
                            entries.push((key, self.value(tag, depth + 1)?));
                        }
                        _ => return None,
                    }
                }
                Plist::Dict(entries)
            }
            "array" => {
                let mut items = Vec::new();
                loop {
                    match self.tag()? {
                        "/array" => break,
                        tag => items.push(self.value(tag, depth + 1)?),
                    }
                }
                Plist::Array(items)
            }
            "dict/" => Plist::Dict(Vec::new()),
            "array/" => Plist::Array(Vec::new()),
            "string" => Plist::String(unescape(self.text("string")?)),
            "string/" => Plist::String(String::new()),
            "integer" => Plist::Integer(self.text("integer")?.trim().parse().ok()?),
            "true/" => Plist::Bool(true),
            "false/" => Plist::Bool(false),
            "data" | "real" | "date" => {
                self.text(tag)?;
                Plist::Other
            }
            "data/" => Plist::Other,
            _ => return None,
        })
    }
}

/// Parses one XML property list.
pub(crate) fn parse(bytes: &[u8]) -> Option<Plist> {
    let text = std::str::from_utf8(bytes).ok()?;
    let start = text.find("<plist")?;
    let mut parser = Parser {
        rest: &text[start..],
    };
    parser.tag()?;
    let tag = parser.tag()?;
    let value = parser.value(tag, 0)?;
    (parser.tag()? == "/plist").then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_and_escapes_its_text() {
        let bytes = request(&[
            ("MessageType", Field::Text("Connect")),
            ("ProgName", Field::Text("a<b>&c")),
            ("PortNumber", Field::Number(52410)),
        ]);
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed.string("MessageType"), Some("Connect"));
        assert_eq!(parsed.string("ProgName"), Some("a<b>&c"));
        assert_eq!(parsed.integer("PortNumber"), Some(52410));
    }

    #[test]
    fn nested_values_and_unknown_kinds_parse() {
        let xml = br#"<?xml version="1.0"?><plist version="1.0"><dict>
            <key>DeviceList</key><array>
              <dict><key>DeviceID</key><integer>7</integer>
                <key>Properties</key><dict>
                  <key>ConnectionType</key><string>USB</string>
                  <key>Blob</key><data>AAEC</data><key>On</key><true/>
                  <key>Empty</key><string/>
                </dict></dict>
            </array></dict></plist>"#;
        let parsed = parse(xml).unwrap();
        let device = &parsed.array("DeviceList").unwrap()[0];
        assert_eq!(device.integer("DeviceID"), Some(7));
        let properties = device.get("Properties").unwrap();
        assert_eq!(properties.string("ConnectionType"), Some("USB"));
        assert_eq!(properties.get("Blob"), Some(&Plist::Other));
        assert_eq!(properties.get("On"), Some(&Plist::Bool(true)));
        assert_eq!(properties.string("Empty"), Some(""));
    }

    #[test]
    fn malformed_or_too_deep_input_is_refused_without_panicking() {
        for bad in [
            &b""[..],
            b"not a plist",
            b"<plist><dict><key>a</key></dict></plist>",
            b"<plist><dict><key>a</key><integer>x</integer></dict></plist>",
            b"<plist><dict><string>no key</string></dict></plist>",
            b"<plist><dict>",
            b"<plist><unknown/></plist>",
            b"\xff\xfe",
        ] {
            assert_eq!(parse(bad), None);
        }
        let deep = format!(
            "<plist>{}{}</plist>",
            "<array>".repeat(64),
            "</array>".repeat(64)
        );
        assert_eq!(parse(deep.as_bytes()), None);
    }
}
