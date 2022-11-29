use super::TextNode;
use crate::prelude::*;

/// A text space.
#[derive(Debug, Hash)]
pub struct SpaceNode;

#[node(Unlabellable, Behave)]
impl SpaceNode {
    fn construct(_: &Vm, _: &mut Args) -> SourceResult<Content> {
        Ok(Self.pack())
    }
}

impl Unlabellable for SpaceNode {}

impl Behave for SpaceNode {
    fn behaviour(&self) -> Behaviour {
        Behaviour::Weak(2)
    }
}

/// A line break.
#[derive(Debug, Hash)]
pub struct LinebreakNode {
    pub justify: bool,
}

#[node(Behave)]
impl LinebreakNode {
    fn construct(_: &Vm, args: &mut Args) -> SourceResult<Content> {
        let justify = args.named("justify")?.unwrap_or(false);
        Ok(Self { justify }.pack())
    }
}

impl Behave for LinebreakNode {
    fn behaviour(&self) -> Behaviour {
        Behaviour::Destructive
    }
}

/// Strong content, rendered in boldface by default.
#[derive(Debug, Hash)]
pub struct StrongNode(pub Content);

#[node(Show)]
impl StrongNode {
    fn construct(_: &Vm, args: &mut Args) -> SourceResult<Content> {
        Ok(Self(args.expect("body")?).pack())
    }

    fn field(&self, name: &str) -> Option<Value> {
        match name {
            "body" => Some(Value::Content(self.0.clone())),
            _ => None,
        }
    }
}

impl Show for StrongNode {
    fn show(&self, _: Tracked<dyn World>, _: StyleChain) -> Content {
        self.0.clone().styled(TextNode::BOLD, Toggle)
    }
}

/// Emphasized content, rendered with an italic font by default.
#[derive(Debug, Hash)]
pub struct EmphNode(pub Content);

#[node(Show)]
impl EmphNode {
    fn construct(_: &Vm, args: &mut Args) -> SourceResult<Content> {
        Ok(Self(args.expect("body")?).pack())
    }

    fn field(&self, name: &str) -> Option<Value> {
        match name {
            "body" => Some(Value::Content(self.0.clone())),
            _ => None,
        }
    }
}

impl Show for EmphNode {
    fn show(&self, _: Tracked<dyn World>, _: StyleChain) -> Content {
        self.0.clone().styled(TextNode::ITALIC, Toggle)
    }
}

/// A toggle that turns on and off alternatingly if folded.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct Toggle;

impl Fold for Toggle {
    type Output = bool;

    fn fold(self, outer: Self::Output) -> Self::Output {
        !outer
    }
}

/// Convert a string or content to lowercase.
pub fn lower(_: &Vm, args: &mut Args) -> SourceResult<Value> {
    case(Case::Lower, args)
}

/// Convert a string or content to uppercase.
pub fn upper(_: &Vm, args: &mut Args) -> SourceResult<Value> {
    case(Case::Upper, args)
}

/// Change the case of text.
fn case(case: Case, args: &mut Args) -> SourceResult<Value> {
    let Spanned { v, span } = args.expect("string or content")?;
    Ok(match v {
        Value::Str(v) => Value::Str(case.apply(&v).into()),
        Value::Content(v) => Value::Content(v.styled(TextNode::CASE, Some(case))),
        v => bail!(span, "expected string or content, found {}", v.type_name()),
    })
}

/// A case transformation on text.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum Case {
    /// Everything is lowercased.
    Lower,
    /// Everything is uppercased.
    Upper,
}

impl Case {
    /// Apply the case to a string.
    pub fn apply(self, text: &str) -> String {
        match self {
            Self::Lower => text.to_lowercase(),
            Self::Upper => text.to_uppercase(),
        }
    }
}

/// Display text in small capitals.
pub fn smallcaps(_: &Vm, args: &mut Args) -> SourceResult<Value> {
    let body: Content = args.expect("content")?;
    Ok(Value::Content(body.styled(TextNode::SMALLCAPS, true)))
}