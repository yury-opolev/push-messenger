//! tpl/1 definitions and the meta-rules they must pass.

use super::strict::{self, Object, StrictValue, at, join};
use super::wording::check_literal;
use super::{FORMAT, MAX_DEFINITION_BYTES, SenderKind, TemplateError, TemplateRef, is_id, is_key};

const MAX_TITLE_CHARS: usize = 80;
const MAX_LABEL_CHARS: usize = 80;
const MAX_HEADING_CHARS: usize = 200;
const MAX_TEXT_CHARS: usize = 500;
const MAX_PARAMS: usize = 16;
const MAX_BLOCKS: usize = 12;
const MAX_STRING_LEN: u32 = 4000;
const MAX_ENUM_VALUES: usize = 16;
const MAX_LIST_ITEMS: u32 = 50;
const MAX_ITEM_LEN: u32 = 500;
const MAX_BUTTONS: usize = 4;
const MAX_CHOICES: usize = 10;
/// Integer bounds stay exact in every client's numbers (JavaScript's safe integers).
const MAX_INT: i64 = (1 << 53) - 1;

/// A checked tpl/1 definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    template: TemplateRef,
    title: String,
    senders: Vec<SenderKind>,
    params: Vec<(String, ParamSpec)>,
    layout: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamSpec {
    pub ty: ParamType,
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamType {
    String { max_len: u32, multiline: bool },
    Enum { values: Vec<String> },
    Int { min: i64, max: i64 },
    Bool,
    List { item: ItemType, max_items: u32 },
}

/// The item type of a `list` param.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemType {
    /// A single-line string.
    String { max_len: u32 },
    /// `{id, label}`.
    Option,
    /// `{label, status, detail?}`.
    Step,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading(Source),
    Text(Source),
    Preview {
        param: String,
        label: Option<String>,
    },
    Thinking {
        param: String,
        label: Option<String>,
    },
    Steps {
        param: String,
        label: Option<String>,
    },
    Buttons {
        options: Vec<ButtonOption>,
    },
    Single {
        label: Option<String>,
        choices: Choices,
    },
    Multi {
        label: Option<String>,
        choices: Choices,
        min: u32,
        max: u32,
    },
    Status {
        param: String,
        label: Option<String>,
    },
    /// Markdown from a multiline string param, checked by `check_markdown`.
    Markdown {
        param: String,
    },
}

/// Literal text, or the name of a param holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Literal(String),
    Param(String),
}

/// Literal options, or the name of a `list` of `option` param.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choices {
    Literal(Vec<ChoiceOption>),
    Param(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonOption {
    pub id: String,
    pub label: String,
    pub style: ButtonStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Secondary,
    Danger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceOption {
    pub id: String,
    pub label: String,
}

impl ButtonStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
            Self::Danger => "danger",
        }
    }
}

impl Block {
    /// The block's `type` tag.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Heading(_) => "heading",
            Self::Text(_) => "text",
            Self::Preview { .. } => "preview",
            Self::Thinking { .. } => "thinking",
            Self::Steps { .. } => "steps",
            Self::Buttons { .. } => "buttons",
            Self::Single { .. } => "single",
            Self::Multi { .. } => "multi",
            Self::Status { .. } => "status",
            Self::Markdown { .. } => "markdown",
        }
    }

    /// Whether the block asks for an answer: `buttons`, `single` or `multi`.
    pub fn is_interactive(&self) -> bool {
        matches!(
            self,
            Self::Buttons { .. } | Self::Single { .. } | Self::Multi { .. }
        )
    }

    /// The param the block reads, if any.
    pub fn param(&self) -> Option<&str> {
        match self {
            Self::Heading(Source::Param(p))
            | Self::Text(Source::Param(p))
            | Self::Preview { param: p, .. }
            | Self::Thinking { param: p, .. }
            | Self::Steps { param: p, .. }
            | Self::Status { param: p, .. }
            | Self::Markdown { param: p }
            | Self::Single {
                choices: Choices::Param(p),
                ..
            }
            | Self::Multi {
                choices: Choices::Param(p),
                ..
            } => Some(p),
            _ => None,
        }
    }
}

impl Definition {
    /// Parses a tpl/1 definition and checks every meta-rule.
    pub fn parse(json: &[u8]) -> Result<Definition, TemplateError> {
        if json.len() > MAX_DEFINITION_BYTES {
            return Err(TemplateError(format!(
                "the definition is {} bytes; the limit is {MAX_DEFINITION_BYTES}",
                json.len()
            )));
        }
        if std::str::from_utf8(json).is_err() {
            return Err(TemplateError("the definition is not valid UTF-8".into()));
        }
        let value = strict::parse(json)
            .map_err(|e| TemplateError(format!("the definition is not strict JSON: {e}")))?;
        let StrictValue::Object(_) = value else {
            return Err(TemplateError("the definition must be a JSON object".into()));
        };
        let root = value.object("")?;
        root.only(&[
            "format",
            "publisher",
            "name",
            "version",
            "title",
            "senders",
            "params",
            "layout",
        ])?;
        let format = root.string("format")?;
        if format != FORMAT {
            return Err(at("format", format!("must be {FORMAT:?}, not {format:?}")));
        }
        let publisher = id(&root, "publisher")?;
        let name = id(&root, "name")?;
        let version = root.integer("version")?;
        let version = u16::try_from(version)
            .ok()
            .filter(|v| *v >= 1)
            .ok_or_else(|| at("version", format!("must be 1 to 65535, not {version}")))?;
        let title = literal(&root, "title", MAX_TITLE_CHARS, false)?;
        let senders = parse_senders(&root)?;
        let params = parse_params(root.field("params")?)?;
        let layout = parse_layout(root.field("layout")?)?;
        let definition = Definition {
            template: TemplateRef {
                publisher,
                name,
                version,
            },
            title,
            senders,
            params,
            layout,
        };
        definition.check_layout()?;
        Ok(definition)
    }

    pub fn template_ref(&self) -> TemplateRef {
        self.template.clone()
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn senders(&self) -> &[SenderKind] {
        &self.senders
    }

    pub fn allows(&self, kind: SenderKind) -> bool {
        self.senders.contains(&kind)
    }

    /// The params, in definition order.
    pub fn params(&self) -> &[(String, ParamSpec)] {
        &self.params
    }

    pub fn param(&self, name: &str) -> Option<&ParamSpec> {
        self.params
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, spec)| spec)
    }

    pub fn layout(&self) -> &[Block] {
        &self.layout
    }

    /// The `buttons`, `single` or `multi` block, if the template has one.
    pub fn interactive(&self) -> Option<&Block> {
        self.layout.iter().find(|b| b.is_interactive())
    }

    /// Compact JSON with a fixed key order and defaults omitted; it parses back to an
    /// equal definition.
    pub fn canonical_json(&self) -> String {
        let senders = self
            .senders
            .iter()
            .map(|s| text(s.as_str()))
            .collect::<Vec<_>>();
        let params = self
            .params
            .iter()
            .map(|(name, spec)| (name.clone(), param_json(spec)))
            .collect();
        object(vec![
            ("format", text(FORMAT)),
            ("publisher", text(&self.template.publisher)),
            ("name", text(&self.template.name)),
            ("version", number(self.template.version.into())),
            ("title", text(&self.title)),
            ("senders", StrictValue::Array(senders)),
            ("params", StrictValue::Object(params)),
            (
                "layout",
                StrictValue::Array(self.layout.iter().map(block_json).collect()),
            ),
        ])
        .to_string()
    }

    /// Binds blocks to params: types, use, interactivity and `multi` bounds.
    fn check_layout(&self) -> Result<(), TemplateError> {
        let mut used = vec![false; self.params.len()];
        let mut interactive: Option<usize> = None;
        for (i, block) in self.layout.iter().enumerate() {
            let path = format!("layout[{i}]");
            if block.is_interactive() {
                if let Some(first) = interactive {
                    return Err(at(
                        &path,
                        format!("a second interactive block (layout[{first}] is one)"),
                    ));
                }
                interactive = Some(i);
            }
            let Some(name) = block.param() else {
                if let Block::Multi {
                    choices: Choices::Literal(options),
                    min,
                    max,
                    ..
                } = block
                {
                    check_bounds(&path, *min, *max, options.len() as u32)?;
                }
                continue;
            };
            let param_path = join(&path, "param");
            let Some(index) = self.params.iter().position(|(n, _)| n == name) else {
                return Err(at(&param_path, format!("no param is named {name:?}")));
            };
            used[index] = true;
            let spec = &self.params[index].1;
            let fits = match (block, &spec.ty) {
                (Block::Heading(_), ParamType::String { multiline, .. }) => !multiline,
                // A text block also shows the values no other block can: numbers, yes/no,
                // enum values and string lists, as plain text.
                (
                    Block::Text(_),
                    ParamType::Int { .. }
                    | ParamType::Bool
                    | ParamType::Enum { .. }
                    | ParamType::List {
                        item: ItemType::String { .. },
                        ..
                    },
                ) => true,
                (
                    Block::Text(_) | Block::Preview { .. } | Block::Thinking { .. },
                    ParamType::String { .. },
                ) => true,
                (
                    Block::Steps { .. },
                    ParamType::List {
                        item: ItemType::Step,
                        ..
                    },
                ) => true,
                (
                    Block::Single { .. } | Block::Multi { .. },
                    ParamType::List {
                        item: ItemType::Option,
                        ..
                    },
                ) => true,
                (Block::Status { .. }, ParamType::Enum { .. }) => true,
                (Block::Markdown { .. }, ParamType::String { multiline, .. }) => *multiline,
                _ => false,
            };
            if !fits {
                let needed = match block {
                    Block::Heading(_) => "a single-line string",
                    Block::Steps { .. } => "a list of step",
                    Block::Single { .. } | Block::Multi { .. } => "a list of option",
                    Block::Status { .. } => "an enum",
                    Block::Markdown { .. } => "a multiline string",
                    Block::Text(_) => "a string, int, bool, enum or list of string",
                    _ => "a string",
                };
                return Err(at(
                    &param_path,
                    format!(
                        "a {} block needs {needed} param; {name:?} is not one",
                        block.kind()
                    ),
                ));
            }
            if block.is_interactive() && spec.optional {
                return Err(at(
                    &param_path,
                    format!("an interactive block cannot use the optional param {name:?}"),
                ));
            }
            if let (Block::Heading(_), ParamType::String { max_len, .. }) = (block, &spec.ty) {
                if *max_len as usize > MAX_HEADING_CHARS {
                    return Err(at(
                        &param_path,
                        format!(
                            "a heading block needs a param with max_len at most \
                             {MAX_HEADING_CHARS}; {name:?} has {max_len}"
                        ),
                    ));
                }
            }
            if let (Block::Multi { min, max, .. }, ParamType::List { max_items, .. }) =
                (block, &spec.ty)
            {
                check_bounds(&path, *min, *max, *max_items)?;
            }
        }
        if let Some(index) = used.iter().position(|u| !u) {
            return Err(at(
                &join("params", &self.params[index].0),
                "is not used by any block",
            ));
        }
        Ok(())
    }
}

fn check_bounds(path: &str, min: u32, max: u32, cap: u32) -> Result<(), TemplateError> {
    if min < 1 || min > max || max > cap {
        return Err(at(
            path,
            format!("needs 1 <= min <= max <= {cap}, not min {min} and max {max}"),
        ));
    }
    Ok(())
}

fn id(obj: &Object<'_>, key: &str) -> Result<String, TemplateError> {
    let value = obj.string(key)?;
    if !is_id(value) {
        return Err(at(
            &obj.path(key),
            format!("{value:?} does not match ^[a-z][a-z0-9-]{{0,31}}$"),
        ));
    }
    Ok(value.to_owned())
}

fn key_at(path: &str, value: &str) -> Result<(), TemplateError> {
    if !is_key(value) {
        return Err(at(
            path,
            format!("{value:?} does not match ^[a-z][a-z0-9_]{{0,31}}$"),
        ));
    }
    Ok(())
}

fn literal(
    obj: &Object<'_>,
    key: &str,
    max_chars: usize,
    newlines: bool,
) -> Result<String, TemplateError> {
    let value = obj.string(key)?;
    check_literal(value, max_chars, newlines).map_err(|e| at(&obj.path(key), e))?;
    Ok(value.to_owned())
}

fn label(obj: &Object<'_>) -> Result<Option<String>, TemplateError> {
    match obj.get("label") {
        Some(_) => literal(obj, "label", MAX_LABEL_CHARS, false).map(Some),
        None => Ok(None),
    }
}

fn bounded(obj: &Object<'_>, key: &str, min: u32, max: u32) -> Result<u32, TemplateError> {
    let value = obj.integer(key)?;
    u32::try_from(value)
        .ok()
        .filter(|v| (min..=max).contains(v))
        .ok_or_else(|| {
            at(
                &obj.path(key),
                format!("must be {min} to {max}, not {value}"),
            )
        })
}

fn count(path: &str, len: usize, min: usize, max: usize, what: &str) -> Result<(), TemplateError> {
    if !(min..=max).contains(&len) {
        return Err(at(path, format!("needs {min} to {max} {what}, not {len}")));
    }
    Ok(())
}

fn parse_senders(root: &Object<'_>) -> Result<Vec<SenderKind>, TemplateError> {
    let path = root.path("senders");
    let items = root.array("senders")?;
    count(&path, items.len(), 1, 2, "senders")?;
    let mut senders = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let item_path = format!("{path}[{i}]");
        let kind = match item.string(&item_path)? {
            "human" => SenderKind::Human,
            "agent" => SenderKind::Agent,
            other => {
                return Err(at(
                    &item_path,
                    format!("must be \"human\" or \"agent\", not {other:?}"),
                ));
            }
        };
        if senders.contains(&kind) {
            return Err(at(&item_path, format!("{kind} is listed twice")));
        }
        senders.push(kind);
    }
    Ok(senders)
}

fn parse_params(value: &StrictValue) -> Result<Vec<(String, ParamSpec)>, TemplateError> {
    let obj = value.object("params")?;
    count("params", obj.fields().len(), 0, MAX_PARAMS, "params")?;
    obj.fields()
        .iter()
        .map(|(name, spec)| {
            let path = join("params", name);
            key_at(&path, name)?;
            Ok((name.clone(), parse_param(spec, &path)?))
        })
        .collect()
}

fn parse_param(value: &StrictValue, path: &str) -> Result<ParamSpec, TemplateError> {
    let obj = value.object(path)?;
    let ty = match obj.string("type")? {
        "string" => {
            obj.only(&["type", "max_len", "multiline", "optional"])?;
            ParamType::String {
                max_len: bounded(&obj, "max_len", 1, MAX_STRING_LEN)?,
                multiline: obj.flag("multiline")?,
            }
        }
        "enum" => {
            obj.only(&["type", "values", "optional"])?;
            let path = obj.path("values");
            let items = obj.array("values")?;
            count(&path, items.len(), 1, MAX_ENUM_VALUES, "values")?;
            let mut values: Vec<String> = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let item_path = format!("{path}[{i}]");
                let value = item.string(&item_path)?;
                key_at(&item_path, value)?;
                if values.iter().any(|v| v == value) {
                    return Err(at(&item_path, format!("{value:?} is listed twice")));
                }
                values.push(value.to_owned());
            }
            ParamType::Enum { values }
        }
        "int" => {
            obj.only(&["type", "min", "max", "optional"])?;
            let min = safe_int(&obj, "min")?;
            let max = safe_int(&obj, "max")?;
            if min > max {
                return Err(at(path, format!("min {min} is above max {max}")));
            }
            ParamType::Int { min, max }
        }
        "bool" => {
            obj.only(&["type", "optional"])?;
            ParamType::Bool
        }
        "list" => {
            obj.only(&["type", "item", "max_items", "optional"])?;
            ParamType::List {
                item: parse_item_type(obj.field("item")?, &obj.path("item"))?,
                max_items: bounded(&obj, "max_items", 1, MAX_LIST_ITEMS)?,
            }
        }
        other => {
            return Err(at(
                &obj.path("type"),
                format!("unknown param type {other:?}"),
            ));
        }
    };
    Ok(ParamSpec {
        ty,
        optional: obj.flag("optional")?,
    })
}

fn safe_int(obj: &Object<'_>, key: &str) -> Result<i64, TemplateError> {
    let value = obj.integer(key)?;
    if !(-MAX_INT..=MAX_INT).contains(&value) {
        return Err(at(
            &obj.path(key),
            format!("must be within ±{MAX_INT}, not {value}"),
        ));
    }
    Ok(value)
}

fn parse_item_type(value: &StrictValue, path: &str) -> Result<ItemType, TemplateError> {
    let obj = value.object(path)?;
    match obj.string("type")? {
        "string" => {
            obj.only(&["type", "max_len"])?;
            Ok(ItemType::String {
                max_len: bounded(&obj, "max_len", 1, MAX_ITEM_LEN)?,
            })
        }
        "option" => {
            obj.only(&["type"])?;
            Ok(ItemType::Option)
        }
        "step" => {
            obj.only(&["type"])?;
            Ok(ItemType::Step)
        }
        other => Err(at(
            &obj.path("type"),
            format!("unknown item type {other:?}"),
        )),
    }
}

fn parse_layout(value: &StrictValue) -> Result<Vec<Block>, TemplateError> {
    let items = value.array("layout")?;
    count("layout", items.len(), 1, MAX_BLOCKS, "blocks")?;
    items
        .iter()
        .enumerate()
        .map(|(i, item)| parse_block(item, &format!("layout[{i}]")))
        .collect()
}

fn parse_block(value: &StrictValue, path: &str) -> Result<Block, TemplateError> {
    let obj = value.object(path)?;
    let kind = obj.string("type")?;
    let block = match kind {
        "heading" | "text" => {
            obj.only(&["type", "text", "param"])?;
            let heading = kind == "heading";
            let source = match (obj.get("text"), obj.get("param")) {
                (Some(_), None) => {
                    let (max, newlines) = if heading {
                        (MAX_HEADING_CHARS, false)
                    } else {
                        (MAX_TEXT_CHARS, true)
                    };
                    Source::Literal(literal(&obj, "text", max, newlines)?)
                }
                (None, Some(_)) => Source::Param(obj.string("param")?.to_owned()),
                _ => return Err(at(path, "needs exactly one of \"text\" and \"param\"")),
            };
            if heading {
                Block::Heading(source)
            } else {
                Block::Text(source)
            }
        }
        "preview" | "thinking" | "steps" | "status" => {
            obj.only(&["type", "param", "label"])?;
            let param = obj.string("param")?.to_owned();
            let label = label(&obj)?;
            match kind {
                "preview" => Block::Preview { param, label },
                "thinking" => Block::Thinking { param, label },
                "steps" => Block::Steps { param, label },
                _ => Block::Status { param, label },
            }
        }
        "markdown" => {
            obj.only(&["type", "param"])?;
            Block::Markdown {
                param: obj.string("param")?.to_owned(),
            }
        }
        "buttons" => {
            obj.only(&["type", "options"])?;
            let path = obj.path("options");
            let items = obj.array("options")?;
            count(&path, items.len(), 1, MAX_BUTTONS, "options")?;
            let mut options: Vec<ButtonOption> = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let item_path = format!("{path}[{i}]");
                let option = item.object(&item_path)?;
                option.only(&["id", "label", "style"])?;
                let id = option_id(&option, options.iter().map(|o| o.id.as_str()))?;
                let label = literal(&option, "label", MAX_LABEL_CHARS, false)?;
                let style = match option.string("style")? {
                    "primary" => ButtonStyle::Primary,
                    "secondary" => ButtonStyle::Secondary,
                    "danger" => ButtonStyle::Danger,
                    other => {
                        return Err(at(
                            &option.path("style"),
                            format!(
                                "must be \"primary\", \"secondary\" or \"danger\", not {other:?}"
                            ),
                        ));
                    }
                };
                options.push(ButtonOption { id, label, style });
            }
            Block::Buttons { options }
        }
        "single" | "multi" => {
            if kind == "single" {
                obj.only(&["type", "label", "options", "param"])?;
            } else {
                obj.only(&["type", "label", "options", "param", "min", "max"])?;
            }
            let label = label(&obj)?;
            let choices = match (obj.get("options"), obj.get("param")) {
                (Some(_), None) => {
                    let path = obj.path("options");
                    let items = obj.array("options")?;
                    count(&path, items.len(), 1, MAX_CHOICES, "options")?;
                    let mut options: Vec<ChoiceOption> = Vec::new();
                    for (i, item) in items.iter().enumerate() {
                        let option = item.object(&format!("{path}[{i}]"))?;
                        option.only(&["id", "label"])?;
                        let id = option_id(&option, options.iter().map(|o| o.id.as_str()))?;
                        let label = literal(&option, "label", MAX_LABEL_CHARS, false)?;
                        options.push(ChoiceOption { id, label });
                    }
                    Choices::Literal(options)
                }
                (None, Some(_)) => Choices::Param(obj.string("param")?.to_owned()),
                _ => return Err(at(path, "needs exactly one of \"options\" and \"param\"")),
            };
            if kind == "single" {
                Block::Single { label, choices }
            } else {
                Block::Multi {
                    label,
                    choices,
                    min: bounded(&obj, "min", 0, u32::MAX)?,
                    max: bounded(&obj, "max", 0, u32::MAX)?,
                }
            }
        }
        other => {
            return Err(at(
                &obj.path("type"),
                format!("unknown block type {other:?}"),
            ));
        }
    };
    Ok(block)
}

fn option_id<'a>(
    option: &Object<'_>,
    mut earlier: impl Iterator<Item = &'a str>,
) -> Result<String, TemplateError> {
    let path = option.path("id");
    let id = option.string("id")?;
    key_at(&path, id)?;
    if earlier.any(|e| e == id) {
        return Err(at(&path, format!("duplicate option id {id:?}")));
    }
    Ok(id.to_owned())
}

fn text(s: &str) -> StrictValue {
    StrictValue::String(s.to_owned())
}

fn number(n: i64) -> StrictValue {
    StrictValue::Number(n.into())
}

fn object(fields: Vec<(&str, StrictValue)>) -> StrictValue {
    StrictValue::Object(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn param_json(spec: &ParamSpec) -> StrictValue {
    let mut fields = match &spec.ty {
        ParamType::String { max_len, multiline } => {
            let mut fields = vec![
                ("type", text("string")),
                ("max_len", number((*max_len).into())),
            ];
            if *multiline {
                fields.push(("multiline", StrictValue::Bool(true)));
            }
            fields
        }
        ParamType::Enum { values } => vec![
            ("type", text("enum")),
            (
                "values",
                StrictValue::Array(values.iter().map(|v| text(v)).collect()),
            ),
        ],
        ParamType::Int { min, max } => vec![
            ("type", text("int")),
            ("min", number(*min)),
            ("max", number(*max)),
        ],
        ParamType::Bool => vec![("type", text("bool"))],
        ParamType::List { item, max_items } => {
            let item = match item {
                ItemType::String { max_len } => object(vec![
                    ("type", text("string")),
                    ("max_len", number((*max_len).into())),
                ]),
                ItemType::Option => object(vec![("type", text("option"))]),
                ItemType::Step => object(vec![("type", text("step"))]),
            };
            vec![
                ("type", text("list")),
                ("item", item),
                ("max_items", number((*max_items).into())),
            ]
        }
    };
    if spec.optional {
        fields.push(("optional", StrictValue::Bool(true)));
    }
    object(fields)
}

fn block_json(block: &Block) -> StrictValue {
    let mut fields = vec![("type", text(block.kind()))];
    let push_label = |fields: &mut Vec<(&str, StrictValue)>, label: &Option<String>| {
        if let Some(label) = label {
            fields.push(("label", text(label)));
        }
    };
    match block {
        Block::Heading(source) | Block::Text(source) => fields.push(match source {
            Source::Literal(t) => ("text", text(t)),
            Source::Param(p) => ("param", text(p)),
        }),
        Block::Preview { param, label }
        | Block::Thinking { param, label }
        | Block::Steps { param, label }
        | Block::Status { param, label } => {
            fields.push(("param", text(param)));
            push_label(&mut fields, label);
        }
        Block::Markdown { param } => fields.push(("param", text(param))),
        Block::Buttons { options } => {
            let options = options
                .iter()
                .map(|o| {
                    object(vec![
                        ("id", text(&o.id)),
                        ("label", text(&o.label)),
                        ("style", text(o.style.as_str())),
                    ])
                })
                .collect();
            fields.push(("options", StrictValue::Array(options)));
        }
        Block::Single { label, choices } | Block::Multi { label, choices, .. } => {
            push_label(&mut fields, label);
            fields.push(match choices {
                Choices::Literal(options) => (
                    "options",
                    StrictValue::Array(
                        options
                            .iter()
                            .map(|o| object(vec![("id", text(&o.id)), ("label", text(&o.label))]))
                            .collect(),
                    ),
                ),
                Choices::Param(p) => ("param", text(p)),
            });
            if let Block::Multi { min, max, .. } = block {
                fields.push(("min", number((*min).into())));
                fields.push(("max", number((*max).into())));
            }
        }
    }
    object(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn parse(value: &Value) -> Result<Definition, TemplateError> {
        Definition::parse(&serde_json::to_vec(value).unwrap())
    }

    fn refused(value: &Value, expected: &str) {
        let error = parse(value)
            .expect_err("the definition should be refused")
            .0;
        assert!(error.contains(expected), "{error:?} lacks {expected:?}");
    }

    fn base() -> Value {
        json!({
            "format": "tpl/1",
            "publisher": "acme",
            "name": "note",
            "version": 1,
            "title": "Note",
            "senders": ["human", "agent"],
            "params": {"body": {"type": "string", "max_len": 100}},
            "layout": [{"type": "text", "param": "body"}]
        })
    }

    fn with_block(block: Value) -> Value {
        let mut value = base();
        value["layout"].as_array_mut().unwrap().push(block);
        value
    }

    fn with_param(name: &str, spec: Value, block: Value) -> Value {
        let mut value = with_block(block);
        value["params"][name] = spec;
        value
    }

    fn buttons(n: usize) -> Value {
        let options: Vec<Value> = (0..n)
            .map(
                |i| json!({"id": format!("b{i}"), "label": format!("B {i}"), "style": "secondary"}),
            )
            .collect();
        json!({"type": "buttons", "options": options})
    }

    fn rich() -> Value {
        json!({
            "format": "tpl/1",
            "publisher": "acme",
            "name": "everything",
            "version": 65535,
            "title": "Everything",
            "senders": ["agent"],
            "params": {
                "title": {"type": "string", "max_len": 120, "optional": true},
                "details": {"type": "string", "max_len": 4000, "multiline": true},
                "thought": {"type": "string", "max_len": 10, "optional": true},
                "steps": {"type": "list", "item": {"type": "step"}, "max_items": 50},
                "state": {"type": "enum", "values": ["a", "b_2"]},
                "count": {"type": "int", "min": -9_007_199_254_740_991_i64, "max": 9_007_199_254_740_991_i64, "optional": true},
                "flag": {"type": "bool"},
                "tags": {"type": "list", "item": {"type": "string", "max_len": 500}, "max_items": 3},
                "choices": {"type": "list", "item": {"type": "option"}, "max_items": 10}
            },
            "layout": [
                {"type": "heading", "param": "title"},
                {"type": "heading", "text": "Static heading"},
                {"type": "text", "text": "Two\nlines"},
                {"type": "text", "param": "details"},
                {"type": "preview", "param": "details", "label": "Preview"},
                {"type": "thinking", "param": "thought"},
                {"type": "steps", "param": "steps", "label": "Steps"},
                {"type": "status", "param": "state"},
                {"type": "text", "param": "count"},
                {"type": "text", "param": "flag"},
                {"type": "text", "param": "tags"},
                {"type": "multi", "label": "Pick", "param": "choices", "min": 1, "max": 10}
            ]
        })
    }

    #[test]
    fn a_minimal_definition_parses() {
        let definition = parse(&base()).unwrap();
        assert_eq!(definition.template_ref().to_string(), "acme.note@1");
        assert_eq!(definition.title(), "Note");
        assert!(definition.allows(SenderKind::Human));
        assert!(definition.allows(SenderKind::Agent));
        assert_eq!(
            definition.param("body"),
            Some(&ParamSpec {
                ty: ParamType::String {
                    max_len: 100,
                    multiline: false,
                },
                optional: false,
            })
        );
        assert_eq!(
            definition.layout(),
            [Block::Text(Source::Param("body".into()))]
        );
        assert_eq!(definition.interactive(), None);
    }

    #[test]
    fn canonical_json_parses_back_to_an_equal_definition() {
        let mut buttons_value = with_block(buttons(2));
        buttons_value["layout"][1]["options"][0]["style"] = json!("primary");
        buttons_value["layout"][1]["options"][1]["style"] = json!("danger");
        for value in [rich(), buttons_value] {
            let definition = parse(&value).unwrap();
            let canonical = definition.canonical_json();
            let again = Definition::parse(canonical.as_bytes()).unwrap();
            assert_eq!(again, definition);
            assert_eq!(again.canonical_json(), canonical);
        }
    }

    #[test]
    fn canonical_json_keeps_param_order_and_omits_defaults() {
        let mut value = base();
        value["params"]["body"]["multiline"] = json!(false);
        value["params"]["body"]["optional"] = json!(false);
        assert_eq!(
            parse(&value).unwrap().canonical_json(),
            concat!(
                r#"{"format":"tpl/1","publisher":"acme","name":"note","version":1,"#,
                r#""title":"Note","senders":["human","agent"],"#,
                r#""params":{"body":{"type":"string","max_len":100}},"#,
                r#""layout":[{"type":"text","param":"body"}]}"#
            )
        );
    }

    #[test]
    fn interactive_blocks_round_trip() {
        for block in [
            json!({"type": "single", "options": [{"id": "a", "label": "A"}]}),
            json!({"type": "multi", "label": "Pick", "options": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}], "min": 1, "max": 2}),
        ] {
            let definition = parse(&with_block(block)).unwrap();
            assert!(definition.interactive().is_some());
            let again = Definition::parse(definition.canonical_json().as_bytes()).unwrap();
            assert_eq!(again, definition);
        }
    }

    #[test]
    fn the_size_limit_is_inclusive() {
        let mut text = serde_json::to_string(&base()).unwrap();
        text.push_str(&" ".repeat(MAX_DEFINITION_BYTES - text.len()));
        assert!(Definition::parse(text.as_bytes()).is_ok());
        text.push(' ');
        let error = Definition::parse(text.as_bytes()).unwrap_err();
        assert!(error.0.contains("bytes"), "{error}");
    }

    #[test]
    fn refuses_invalid_utf8_and_non_objects() {
        assert!(Definition::parse(&[0xff]).unwrap_err().0.contains("UTF-8"));
        assert!(Definition::parse(b"[]").unwrap_err().0.contains("object"));
    }

    #[test]
    fn refuses_another_format_and_bad_ids() {
        let mut value = base();
        value["format"] = json!("tpl/2");
        refused(&value, "format: must be");
        for (key, bad) in [
            ("publisher", json!("Acme")),
            ("name", json!("a.b")),
            ("version", json!(0)),
            ("version", json!(65536)),
            ("version", json!(1.0)),
            ("version", json!("1")),
        ] {
            let mut value = base();
            value[key] = bad;
            refused(&value, key);
        }
    }

    #[test]
    fn checks_title_and_senders() {
        let mut value = base();
        value["title"] = json!("a".repeat(80));
        assert!(parse(&value).is_ok());
        value["title"] = json!("a".repeat(81));
        refused(&value, "title: is 81 characters");
        value["title"] = json!("Visit example.com");
        refused(&value, "title: looks like a URL");
        for senders in [
            json!([]),
            json!(["human", "human"]),
            json!(["robot"]),
            json!(["human", "agent", "human"]),
        ] {
            let mut value = base();
            value["senders"] = senders;
            refused(&value, "senders");
        }
    }

    #[test]
    fn params_are_capped_at_sixteen() {
        let mut value = base();
        let mut params = serde_json::Map::new();
        for i in 0..16 {
            params.insert(format!("p{i}"), json!({"type": "string", "max_len": 1}));
        }
        let layout: Vec<Value> = (0..12)
            .map(|i| json!({"type": "text", "param": format!("p{i}")}))
            .collect();
        value["params"] = Value::Object(params.clone());
        value["layout"] = Value::Array(layout);
        // Twelve blocks bind at most twelve params, so sixteen pass the cap and then fail
        // the use rule.
        refused(&value, "params.p12: is not used by any block");
        params.insert("p16".into(), json!({"type": "string", "max_len": 1}));
        value["params"] = Value::Object(params);
        refused(&value, "params: needs 0 to 16 params, not 17");
    }

    #[test]
    fn layouts_are_capped_at_twelve_blocks() {
        let mut value = base();
        let mut layout = vec![json!({"type": "text", "param": "body"})];
        layout.extend((1..12).map(|_| json!({"type": "text", "text": "Hi"})));
        value["layout"] = Value::Array(layout.clone());
        assert!(parse(&value).is_ok());
        layout.push(json!({"type": "text", "text": "Hi"}));
        value["layout"] = Value::Array(layout);
        refused(&value, "layout: needs 1 to 12 blocks, not 13");
        value["layout"] = json!([]);
        refused(&value, "layout: needs 1 to 12 blocks, not 0");
    }

    #[test]
    fn buttons_are_capped_at_four() {
        assert!(parse(&with_block(buttons(4))).is_ok());
        refused(
            &with_block(buttons(5)),
            "layout[1].options: needs 1 to 4 options, not 5",
        );
        refused(&with_block(buttons(0)), "needs 1 to 4 options, not 0");
    }

    #[test]
    fn literal_choices_are_capped_at_ten() {
        let options = |n: usize| -> Vec<Value> {
            (0..n)
                .map(|i| json!({"id": format!("o{i}"), "label": "O"}))
                .collect()
        };
        assert!(
            parse(&with_block(
                json!({"type": "single", "options": options(10)})
            ))
            .is_ok()
        );
        refused(
            &with_block(json!({"type": "single", "options": options(11)})),
            "needs 1 to 10 options, not 11",
        );
    }

    #[test]
    fn string_max_len_is_capped_at_4000() {
        let mut value = base();
        value["params"]["body"]["max_len"] = json!(4000);
        assert!(parse(&value).is_ok());
        value["params"]["body"]["max_len"] = json!(4001);
        refused(&value, "params.body.max_len: must be 1 to 4000, not 4001");
        value["params"]["body"]["max_len"] = json!(0);
        refused(&value, "params.body.max_len: must be 1 to 4000, not 0");
    }

    #[test]
    fn enums_are_capped_at_sixteen_distinct_values() {
        let values = |n: usize| -> Vec<String> { (0..n).map(|i| format!("v{i}")).collect() };
        let block = json!({"type": "status", "param": "state"});
        let spec = |n| json!({"type": "enum", "values": values(n)});
        assert!(parse(&with_param("state", spec(16), block.clone())).is_ok());
        refused(
            &with_param("state", spec(17), block.clone()),
            "params.state.values: needs 1 to 16 values, not 17",
        );
        refused(
            &with_param(
                "state",
                json!({"type": "enum", "values": ["a", "a"]}),
                block.clone(),
            ),
            "params.state.values[1]: \"a\" is listed twice",
        );
        refused(
            &with_param("state", json!({"type": "enum", "values": ["A"]}), block),
            "params.state.values[0]",
        );
    }

    #[test]
    fn int_bounds_are_checked() {
        let block = json!({"type": "text", "text": "Number"});
        let mut value = with_param(
            "n",
            json!({"type": "int", "min": 2, "max": 1}),
            block.clone(),
        );
        refused(&value, "params.n: min 2 is above max 1");
        value["params"]["n"] = json!({"type": "int", "min": 0, "max": 9_007_199_254_740_992_i64});
        refused(&value, "params.n.max: must be within");
    }

    #[test]
    fn lists_are_capped_at_fifty_items_of_up_to_500_characters() {
        let block = json!({"type": "steps", "param": "steps"});
        let spec = |n: u32| json!({"type": "list", "item": {"type": "step"}, "max_items": n});
        assert!(parse(&with_param("steps", spec(50), block.clone())).is_ok());
        refused(
            &with_param("steps", spec(51), block.clone()),
            "params.steps.max_items: must be 1 to 50, not 51",
        );
        let strings = |n: u32| json!({"type": "list", "item": {"type": "string", "max_len": n}, "max_items": 1});
        let text = json!({"type": "text", "param": "tags"});
        assert!(parse(&with_param("tags", strings(500), text.clone())).is_ok());
        refused(
            &with_param("tags", strings(501), text),
            "params.tags.item.max_len: must be 1 to 500, not 501",
        );
    }

    #[test]
    fn labels_are_capped_at_eighty_characters() {
        let block = |label: String| json!({"type": "buttons", "options": [{"id": "ok", "label": label, "style": "primary"}]});
        assert!(parse(&with_block(block("a".repeat(80)))).is_ok());
        refused(
            &with_block(block("a".repeat(81))),
            "layout[1].options[0].label: is 81 characters; the limit is 80",
        );
    }

    #[test]
    fn refuses_unknown_fields_everywhere() {
        let mut value = base();
        value["extra"] = json!(1);
        refused(&value, "unknown field \"extra\"");
        let mut value = base();
        value["params"]["body"]["colour"] = json!("red");
        refused(&value, "params.body: unknown field \"colour\"");
        let mut value = base();
        value["layout"][0]["label"] = json!("Body");
        refused(&value, "layout[0]: unknown field \"label\"");
        let mut value = with_block(buttons(1));
        value["layout"][1]["options"][0]["icon"] = json!("x");
        refused(&value, "layout[1].options[0]: unknown field \"icon\"");
    }

    #[test]
    fn refuses_unknown_types_and_missing_fields() {
        refused(
            &with_block(json!({"type": "image", "param": "body"})),
            "unknown block type \"image\"",
        );
        let mut value = base();
        value["params"]["body"]["type"] = json!("float");
        refused(&value, "params.body.type: unknown param type \"float\"");
        let mut value = base();
        value.as_object_mut().unwrap().remove("title");
        refused(&value, "missing field \"title\"");
        refused(
            &with_block(json!({"type": "heading"})),
            "layout[1]: needs exactly one",
        );
        refused(
            &with_block(json!({"type": "heading", "text": "A", "param": "body"})),
            "layout[1]: needs exactly one",
        );
    }

    #[test]
    fn refuses_duplicate_keys() {
        let top = r#"{"format":"tpl/1","format":"tpl/1","publisher":"a","name":"b","version":1,"title":"T","senders":["human"],"params":{},"layout":[{"type":"text","text":"Hi"}]}"#;
        let nested = r#"{"format":"tpl/1","publisher":"a","name":"b","version":1,"title":"T","senders":["human"],"params":{},"layout":[{"type":"text","text":"Hi","text":"Ho"}]}"#;
        for text in [top, nested] {
            let error = Definition::parse(text.as_bytes()).unwrap_err().0;
            assert!(error.contains("duplicate key"), "{error}");
        }
    }

    #[test]
    fn refuses_urls_markup_and_bidi_in_literals() {
        refused(
            &with_block(json!({"type": "heading", "text": "Go to www.evil"})),
            "layout[1].text: looks like a URL",
        );
        let mut value = with_block(buttons(1));
        value["layout"][1]["options"][0]["label"] = json!("https://evil.example");
        refused(&value, "layout[1].options[0].label: looks like a URL");
        refused(
            &with_block(json!({"type": "text", "text": "<b>bold</b>"})),
            "layout[1].text: contains markup ('<')",
        );
        refused(
            &with_block(json!({"type": "preview", "param": "body", "label": "ab\u{202E}c"})),
            "layout[1].label: contains the invisible or formatting character U+202E",
        );
        assert!(
            parse(&with_block(
                json!({"type": "heading", "text": "a".repeat(200)})
            ))
            .is_ok()
        );
        refused(
            &with_block(json!({"type": "heading", "text": "a".repeat(201)})),
            "is 201 characters",
        );
        refused(
            &with_block(json!({"type": "heading", "text": "a\nb"})),
            "control character U+000A",
        );
        assert!(
            parse(&with_block(
                json!({"type": "text", "text": "a".repeat(500)})
            ))
            .is_ok()
        );
        refused(
            &with_block(json!({"type": "text", "text": "a".repeat(501)})),
            "is 501 characters",
        );
    }

    #[test]
    fn refuses_an_unused_param() {
        let mut value = base();
        value["params"]["spare"] = json!({"type": "bool"});
        refused(&value, "params.spare: is not used by any block");
    }

    #[test]
    fn refuses_a_missing_or_mistyped_param() {
        refused(
            &with_block(json!({"type": "thinking", "param": "nope"})),
            "layout[1].param: no param is named \"nope\"",
        );
        refused(
            &with_block(json!({"type": "steps", "param": "body"})),
            "a steps block needs a list of step param",
        );
        refused(
            &with_block(json!({"type": "status", "param": "body"})),
            "a status block needs an enum param",
        );
        refused(
            &with_block(json!({"type": "single", "param": "body"})),
            "a single block needs a list of option param",
        );
    }

    #[test]
    fn refuses_a_heading_bound_to_a_multiline_string() {
        let mut value = with_block(json!({"type": "heading", "param": "body"}));
        assert!(parse(&value).is_ok());
        value["params"]["body"]["multiline"] = json!(true);
        refused(&value, "a heading block needs a single-line string param");
    }

    #[test]
    fn refuses_a_heading_bound_to_a_long_string() {
        let mut value = with_block(json!({"type": "heading", "param": "body"}));
        value["params"]["body"]["max_len"] = json!(200);
        assert!(parse(&value).is_ok());
        value["params"]["body"]["max_len"] = json!(201);
        refused(
            &value,
            "layout[1].param: a heading block needs a param with max_len at most 200; \"body\" has 201",
        );
    }

    #[test]
    fn refuses_two_interactive_blocks() {
        let mut value = with_block(buttons(1));
        value["layout"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "single", "options": [{"id": "a", "label": "A"}]}));
        refused(
            &value,
            "layout[2]: a second interactive block (layout[1] is one)",
        );
    }

    #[test]
    fn refuses_duplicate_and_malformed_option_ids() {
        let mut value = with_block(buttons(2));
        value["layout"][1]["options"][1]["id"] = json!("b0");
        refused(
            &value,
            "layout[1].options[1].id: duplicate option id \"b0\"",
        );
        value["layout"][1]["options"][1]["id"] = json!("B1");
        refused(&value, "layout[1].options[1].id");
        value["layout"][1]["options"][1]["id"] = json!("b1");
        value["layout"][1]["options"][1]["style"] = json!("loud");
        refused(&value, "layout[1].options[1].style");
    }

    #[test]
    fn interactive_params_cannot_be_optional() {
        let spec =
            json!({"type": "list", "item": {"type": "option"}, "max_items": 3, "optional": true});
        refused(
            &with_param(
                "choices",
                spec,
                json!({"type": "single", "param": "choices"}),
            ),
            "cannot use the optional param \"choices\"",
        );
    }

    #[test]
    fn multi_bounds_fit_the_options() {
        let literal = |min: u32, max: u32| {
            with_block(
                json!({"type": "multi", "options": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}], "min": min, "max": max}),
            )
        };
        assert!(parse(&literal(1, 2)).is_ok());
        assert!(parse(&literal(2, 2)).is_ok());
        for (min, max) in [(0, 1), (2, 1), (1, 3)] {
            refused(&literal(min, max), "layout[1]: needs 1 <= min <= max <= 2");
        }
        let spec = json!({"type": "list", "item": {"type": "option"}, "max_items": 5});
        let block = |max: u32| json!({"type": "multi", "param": "choices", "min": 1, "max": max});
        assert!(parse(&with_param("choices", spec.clone(), block(5))).is_ok());
        refused(
            &with_param("choices", spec, block(6)),
            "layout[1]: needs 1 <= min <= max <= 5",
        );
    }

    fn markdown(spec: Value) -> Value {
        with_param("notes", spec, json!({"type": "markdown", "param": "notes"}))
    }

    #[test]
    fn a_markdown_block_parses_and_round_trips() {
        let definition = parse(&markdown(
            json!({"type": "string", "max_len": 4000, "multiline": true}),
        ))
        .unwrap();
        let block = &definition.layout()[1];
        assert_eq!(
            block,
            &Block::Markdown {
                param: "notes".into()
            }
        );
        assert_eq!(block.kind(), "markdown");
        assert_eq!(block.param(), Some("notes"));
        assert!(!block.is_interactive());
        let canonical = definition.canonical_json();
        assert!(
            canonical.contains(r#"{"type":"markdown","param":"notes"}"#),
            "{canonical}"
        );
        assert_eq!(Definition::parse(canonical.as_bytes()).unwrap(), definition);
    }

    #[test]
    fn a_markdown_block_takes_only_a_multiline_string_param() {
        let multiline = json!({"type": "string", "max_len": 100, "multiline": true});
        let mut value = markdown(multiline.clone());
        value["layout"][1]["label"] = json!("Notes");
        refused(&value, "layout[1]: unknown field \"label\"");
        let mut value = markdown(multiline);
        value["layout"][1]["text"] = json!("**hi**");
        refused(&value, "layout[1]: unknown field \"text\"");
        refused(
            &with_block(json!({"type": "markdown", "text": "**hi**"})),
            "layout[1]: unknown field \"text\"",
        );
        refused(
            &markdown(json!({"type": "string", "max_len": 100})),
            "layout[1].param: a markdown block needs a multiline string param; \"notes\" is not one",
        );
        refused(
            &markdown(json!({"type": "int", "min": 0, "max": 9})),
            "a markdown block needs a multiline string param",
        );
    }

    #[test]
    fn a_definition_with_every_block_parses() {
        let definition = parse(&rich()).unwrap();
        assert_eq!(definition.layout().len(), 12);
        assert_eq!(definition.params().len(), 9);
        assert!(matches!(
            definition.interactive(),
            Some(Block::Multi { max: 10, .. })
        ));
        assert!(!definition.allows(SenderKind::Human));
    }
}
