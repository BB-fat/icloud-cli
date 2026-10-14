//! Minimal, lossless iCalendar / vCard component model (RFC 5545 / 6350 content lines).
//! Unknown properties are preserved on round-trip so updates never drop data written by Apple clients.

#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub value: String,
}

impl Prop {
    pub fn new(name: &str, value: impl Into<String>) -> Self {
        Prop {
            name: name.to_ascii_uppercase(),
            params: vec![],
            value: value.into(),
        }
    }
    pub fn with_param(mut self, k: &str, v: &str) -> Self {
        self.params.push((k.to_ascii_uppercase(), v.to_string()));
        self
    }
    pub fn param(&self, k: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Component {
    pub name: String,
    pub props: Vec<Prop>,
    pub children: Vec<Component>,
}

impl Component {
    pub fn new(name: &str) -> Self {
        Component {
            name: name.to_ascii_uppercase(),
            ..Default::default()
        }
    }
    pub fn get(&self, name: &str) -> Option<&Prop> {
        self.props
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Prop> + 'a {
        self.props
            .iter()
            .filter(move |p| p.name.eq_ignore_ascii_case(name))
    }
    pub fn text(&self, name: &str) -> Option<String> {
        self.get(name).map(|p| unescape(&p.value))
    }
    pub fn remove(&mut self, name: &str) {
        self.props.retain(|p| !p.name.eq_ignore_ascii_case(name));
    }
    pub fn set(&mut self, prop: Prop) {
        let name = prop.name.clone();
        if let Some(i) = self
            .props
            .iter()
            .position(|p| p.name.eq_ignore_ascii_case(&name))
        {
            self.remove(&name);
            self.props.insert(i.min(self.props.len()), prop);
        } else {
            self.props.push(prop);
        }
    }
    pub fn push(&mut self, prop: Prop) {
        self.props.push(prop);
    }
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Component> + 'a {
        self.children
            .iter()
            .filter(move |c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn serialize(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }
    fn write(&self, out: &mut String) {
        fold_into(out, &format!("BEGIN:{}", self.name));
        for p in &self.props {
            let mut line = p.name.clone();
            for (k, v) in &p.params {
                line.push(';');
                line.push_str(k);
                line.push('=');
                if v.contains([':', ';', ',']) {
                    line.push('"');
                    line.push_str(v);
                    line.push('"');
                } else {
                    line.push_str(v);
                }
            }
            line.push(':');
            line.push_str(&p.value);
            fold_into(out, &line);
        }
        for c in &self.children {
            c.write(out);
        }
        fold_into(out, &format!("END:{}", self.name));
    }
}

/// Fold a content line at 75 octets without splitting UTF-8 sequences.
fn fold_into(out: &mut String, line: &str) {
    let mut count = 0usize;
    for ch in line.chars() {
        let len = ch.len_utf8();
        if count + len > 75 {
            out.push_str("\r\n ");
            count = 1;
        }
        out.push(ch);
        count += len;
    }
    out.push_str("\r\n");
}

pub fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace("\r\n", "\\n")
        .replace('\n', "\\n")
}

pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(o) => out.push(o),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_line(line: &str) -> Option<Prop> {
    // name *(;param) : value  — params may be quoted and contain ':' or ';'
    let mut in_q = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_q = !in_q,
            ':' if !in_q => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = Vec::new();
    let mut cur = String::new();
    in_q = false;
    for c in head.chars() {
        match c {
            '"' => in_q = !in_q,
            ';' if !in_q => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    let mut it = parts.into_iter();
    let mut name = it.next()?.to_ascii_uppercase();
    // vCard group prefix (item1.EMAIL) -> keep name only
    if let Some((_, n)) = name.rsplit_once('.') {
        name = n.to_string();
    }
    let params = it
        .filter_map(|p| {
            p.split_once('=')
                .map(|(k, v)| (k.to_ascii_uppercase(), v.to_string()))
        })
        .collect();
    Some(Prop {
        name,
        params,
        value: value.to_string(),
    })
}

/// Parse text into top-level components (e.g. one VCALENDAR, or several VCARDs).
pub fn parse(text: &str) -> Result<Vec<Component>, String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if (raw.starts_with(' ') || raw.starts_with('\t')) && !lines.is_empty() {
            lines.last_mut().unwrap().push_str(&raw[1..]);
        } else if !raw.is_empty() {
            lines.push(raw.to_string());
        }
    }
    let mut stack: Vec<Component> = Vec::new();
    let mut tops = Vec::new();
    for l in lines {
        let Some(p) = parse_line(&l) else { continue };
        if p.name == "BEGIN" {
            stack.push(Component::new(&p.value));
        } else if p.name == "END" {
            let c = stack.pop().ok_or("unbalanced END")?;
            if !c.name.eq_ignore_ascii_case(&p.value) {
                return Err(format!("END:{} does not match BEGIN:{}", p.value, c.name));
            }
            match stack.last_mut() {
                Some(parent) => parent.children.push(c),
                None => tops.push(c),
            }
        } else if let Some(cur) = stack.last_mut() {
            cur.props.push(p);
        }
    }
    if !stack.is_empty() {
        return Err("unterminated component".into());
    }
    Ok(tops)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_fold() {
        let long = "长".repeat(60);
        let src = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:a\r\nSUMMARY:{}\r\nX-APPLE-STRUCTURED-LOCATION;VALUE=URI;X-TITLE=\"a:b;c\":geo:1,2\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            escape(&long)
        );
        let c = parse(&src).unwrap();
        let ser = c[0].serialize();
        for line in ser.split("\r\n") {
            assert!(line.len() <= 75, "{line}");
        }
        let c2 = parse(&ser).unwrap();
        assert_eq!(c, c2);
        let ev = &c2[0].children[0];
        assert_eq!(ev.text("SUMMARY").unwrap(), long);
        assert_eq!(
            ev.get("X-APPLE-STRUCTURED-LOCATION")
                .unwrap()
                .param("X-TITLE"),
            Some("a:b;c")
        );
    }
    #[test]
    fn escaping() {
        let s = "a,b;c\\d\ne";
        assert_eq!(unescape(&escape(s)), s);
    }
}
