//! The command line a command parses its arguments from, and the usage it prints,
//! both read off the JSON schema of the arguments the command takes.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

fn push_help_line(help: &mut String, line: &str) {
    help.push_str(line);
    help.push('\n');
}

fn render_positional_usage(name: &str, required: bool, variadic: bool) -> String {
    let kebab = name.replace('_', "-");
    let mut output = String::with_capacity(kebab.len() + 7);
    if !required {
        output.push('[');
    }
    output.push('<');
    output.push_str(kebab.as_str());
    output.push('>');
    if variadic {
        output.push_str("...");
    }
    if !required {
        output.push(']');
    }
    output
}

fn render_repeatable_usage(name: &str) -> String {
    let kebab = name.replace('_', "-");
    let mut output = String::with_capacity(kebab.len() + 12);
    output.push_str("--");
    output.push_str(kebab.as_str());
    output.push_str(" <value>...");
    output
}

fn render_flag_prefix(flag: &str, short: Option<char>) -> String {
    let mut output = String::with_capacity(flag.len() + 8);
    output.push_str("  ");
    if let Some(short) = short {
        output.push('-');
        output.push(short);
        output.push_str(", ");
    }
    output.push_str("--");
    output.push_str(flag);
    output
}

/// Converts CLI arguments to JSON using schema information.
///
/// Handles:
/// - Tagged enums (`#[serde(tag = "operation")]`) → subcommands
/// - Long options → `--flag value`, `--flag=value`, `--no-flag`
/// - Short options → `-f`, clusters like `-abc`, and values `-oVALUE` / `-o=VALUE`
/// - `--` to end option parsing
/// - Struct fields → `--flag value` or positional args
/// - Optional fields → optional flags
///
/// # Errors
///
/// Returns an error if the arguments don't match the schema, or if they ask
/// for help: the caller answers [`requests_help`] before it parses.
pub fn cli_to_json(schema: &Value, args: &[String]) -> anyhow::Result<Value> {
    match parse_args(schema, args)? {
        Parsed::Args(value) => Ok(value),
        Parsed::Help => anyhow::bail!("help requested"),
    }
}

/// What parsing a command line found.
#[derive(Debug)]
enum Parsed {
    /// A `-h` or `--help` in option position, found before anything else
    /// could fail.
    Help,
    /// The arguments as a JSON value.
    Args(Value),
}

/// Whether an option parser consumed an option or met a help request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionOutcome {
    Consumed,
    Help,
}

/// Parses `args` against the command's schema.
fn parse_args(schema: &Value, args: &[String]) -> anyhow::Result<Parsed> {
    // Check if this is a tagged enum (oneOf with discriminator)
    if let Some(one_of) = schema.get("oneOf").and_then(Value::as_array) {
        // Look for serde tag annotation in schema
        if let Some(tag) = find_serde_tag(schema) {
            return parse_tagged_enum(schema, one_of, &tag, args);
        }
    }

    // Otherwise treat as a simple object
    parse_object(schema, schema, args)
}

/// Finds the serde tag field name from schema extensions.
fn find_serde_tag(schema: &Value) -> Option<String> {
    // Check for discriminator property (OpenAPI style)
    if let Some(disc) = schema.get("discriminator")
        && let Some(prop) = disc.get("propertyName").and_then(Value::as_str)
    {
        return Some(prop.to_string());
    }

    // Check oneOf variants for common const field (serde tag pattern)
    if let Some(variants) = schema.get("oneOf").and_then(Value::as_array)
        && let Some(first) = variants.first()
        && let Some(props) = first.get("properties").and_then(Value::as_object)
    {
        for (name, prop) in props {
            // If this property has a const value, it's likely the tag
            if prop.get("const").is_some() || prop.get("enum").is_some() {
                return Some(name.clone());
            }
        }
    }

    None
}

/// Parses a tagged enum from CLI arguments.
fn parse_tagged_enum(
    root_schema: &Value,
    variants: &[Value],
    tag: &str,
    args: &[String],
) -> anyhow::Result<Parsed> {
    let terminated = args.first().map(std::string::String::as_str) == Some("--");
    let args = if terminated { &args[1..] } else { args };

    if args.is_empty() {
        let variant_names: Vec<_> = variants
            .iter()
            .filter_map(|v| get_variant_name(v, tag))
            .collect();
        anyhow::bail!("expected subcommand: {}", variant_names.join(", "));
    }

    let subcommand = &args[0];
    let remaining = &args[1..];

    if !terminated && (subcommand == "-h" || subcommand == "--help") {
        return Ok(Parsed::Help);
    }

    // Find matching variant
    for variant in variants {
        if let Some(name) = get_variant_name(variant, tag)
            && name.eq_ignore_ascii_case(subcommand)
        {
            let mut variant_schema = variant.clone();
            remove_required_field(&mut variant_schema, tag);
            // Parse the variant's fields
            let Parsed::Args(mut result) = parse_object(root_schema, &variant_schema, remaining)?
            else {
                return Ok(Parsed::Help);
            };

            // Add the tag field
            if let Value::Object(ref mut map) = result {
                map.insert(tag.to_string(), Value::String(name));
            }

            return Ok(Parsed::Args(result));
        }
    }

    let variant_names: Vec<_> = variants
        .iter()
        .filter_map(|v| get_variant_name(v, tag))
        .collect();
    anyhow::bail!(
        "unknown subcommand '{}', expected one of: {}",
        subcommand,
        variant_names.join(", ")
    );
}

fn remove_required_field(schema: &mut Value, field: &str) {
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|entry| entry.as_str() != Some(field));
    }
}

/// Gets the variant name from a schema object.
fn get_variant_name(schema: &Value, tag: &str) -> Option<String> {
    // Look for const value in tag property
    if let Some(props) = schema.get("properties").and_then(Value::as_object)
        && let Some(prop) = props.get(tag)
    {
        if let Some(const_val) = prop.get("const").and_then(Value::as_str) {
            return Some(const_val.to_string());
        }
        // Also check enum with single value
        if let Some(enum_vals) = prop.get("enum").and_then(Value::as_array)
            && enum_vals.len() == 1
        {
            return enum_vals[0].as_str().map(String::from);
        }
    }

    // Fallback: use title
    schema
        .get("title")
        .and_then(Value::as_str)
        .map(String::from)
}

/// Whether `args` ask for the command's usage.
///
/// Help is `-h` or `--help` in option position: an option's value, a word
/// after `--` and text piped on stdin are never read as flags. `schema` is
/// the command's argument schema, which says which options take a value; a
/// command without one (a raw handler) takes no options this module knows,
/// so only a bare `-h` or `--help` counts.
pub(super) fn requests_help(schema: Option<&Value>, args: &[String]) -> bool {
    let Some(schema) = schema else {
        return args
            .iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "-h" || arg == "--help");
    };
    // A parse error before any help flag means help was not requested: the
    // command runs and reports the error itself.
    matches!(parse_args(schema, args), Ok(Parsed::Help))
}

fn build_short_option_maps(
    properties: &serde_json::Map<String, Value>,
) -> (HashMap<char, String>, HashMap<String, char>) {
    let mut taken = HashSet::new();
    taken.insert('h');
    let mut short_to_field = HashMap::new();
    let mut field_to_short = HashMap::new();

    for field in properties.keys() {
        let long = field.replace('_', "-");
        if let Some(ch) = pick_short_option(&long, &taken) {
            taken.insert(ch);
            short_to_field.insert(ch, field.clone());
            field_to_short.insert(field.clone(), ch);
        }
    }

    (short_to_field, field_to_short)
}

fn pick_short_option(long: &str, taken: &HashSet<char>) -> Option<char> {
    for ch in long.chars() {
        if !ch.is_ascii_alphabetic() {
            continue;
        }
        let lower = ch.to_ascii_lowercase();
        if !taken.contains(&lower) {
            return Some(lower);
        }
    }
    None
}

/// Finds the most similar option name for typo suggestions.
fn find_similar_option<'a>(input: &str, options: impl Iterator<Item = &'a str>) -> Option<String> {
    let input_lower = input.to_lowercase().replace('_', "-");
    options
        .map(|opt| (opt, strsim::levenshtein(&input_lower, &opt.to_lowercase())))
        .filter(|(_, dist)| *dist <= 2) // Only suggest if edit distance <= 2
        .min_by_key(|(_, dist)| *dist)
        .map(|(opt, _)| opt.to_string())
}

/// Parses an object schema from CLI arguments.
fn parse_object(root_schema: &Value, schema: &Value, args: &[String]) -> anyhow::Result<Parsed> {
    let schema = ObjectParseSchema::new(root_schema, schema)?;
    let mut state = ObjectParseState::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if !state.end_of_options && arg == "--" {
            state.end_of_options = true;
            i += 1;
            continue;
        }

        if !state.end_of_options && arg.starts_with("--") {
            if parse_long_option(root_schema, args, &mut i, &schema, &mut state)?
                == OptionOutcome::Help
            {
                return Ok(Parsed::Help);
            }
        } else if !state.end_of_options
            && arg.starts_with('-')
            && arg.len() > 1
            && arg != "-"
            && !is_negative_number_operand(root_schema, &schema, &state, arg)
        {
            let outcome = parse_short_options(
                root_schema,
                arg,
                args,
                &mut i,
                &schema.properties,
                &schema.short_to_field,
                &mut state.result,
            )?;
            if outcome == OptionOutcome::Help {
                return Ok(Parsed::Help);
            }
        } else {
            parse_positional_arg(root_schema, &schema, &mut state, arg)?;
        }

        i += 1;
    }

    for req in &schema.required {
        if !state.result.contains_key(req) {
            anyhow::bail!("missing required argument: {req}");
        }
    }

    Ok(Parsed::Args(Value::Object(
        state.result.into_iter().collect(),
    )))
}

struct ObjectParseSchema {
    properties: serde_json::Map<String, Value>,
    required: Vec<String>,
    positional_fields: Vec<String>,
    short_to_field: HashMap<char, String>,
    long_names: Vec<String>,
}

impl ObjectParseSchema {
    fn new(root_schema: &Value, schema: &Value) -> Result<Self, PositionalLayoutError> {
        let resolved = resolved_schema(root_schema, schema);
        check_object_layout(root_schema, resolved)?;
        let (properties, required) = object_fields(resolved);
        let positional_fields = positional_fields(root_schema, &properties, &required);
        let (short_to_field, _) = build_short_option_maps(&properties);
        let long_names = properties.keys().map(|k| k.replace('_', "-")).collect();
        Ok(Self {
            properties,
            required,
            positional_fields,
            short_to_field,
            long_names,
        })
    }
}

#[derive(Default)]
struct ObjectParseState {
    result: HashMap<String, Value>,
    positional_idx: usize,
    end_of_options: bool,
}

/// The schema keyword that marks an optional field positional.
///
/// A tool's argument schema binds bare words, in order, to its required
/// non-array fields. Setting this keyword to `true` on a field's schema
/// binds words to that field too, after the required ones, though the field
/// is optional:
///
/// ```ignore
/// #[derive(JsonSchema)]
/// struct Args {
///     question: String,
///     #[schemars(extend("x-positional" = true))]
///     context: Option<String>,
/// }
/// ```
///
/// An array field is filled by repeated flags and is not positional unless
/// it carries this keyword. A marked array field is variadic: it collects
/// every bare word left after the other positional fields, each parsed by
/// the array's item type, and shows in usage as `<name>...`. At most one
/// field is variadic and it must be the last positional field; a schema
/// that breaks this is rejected when the command is built
/// ([`validate_positional_layout`]).
pub const POSITIONAL_KEYWORD: &str = "x-positional";

/// A schema whose variadic positional field breaks the layout rules.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PositionalLayoutError {
    /// More than one array field is positional.
    #[error("at most one positional field can be variadic, found `{first}` and `{second}`")]
    MultipleVariadic {
        /// The first variadic field.
        first: String,
        /// The second variadic field.
        second: String,
    },
    /// A positional field follows the variadic one.
    #[error("variadic positional field `{variadic}` must come last, but `{after}` follows it")]
    VariadicNotLast {
        /// The variadic field.
        variadic: String,
        /// The positional field after it.
        after: String,
    },
}

/// Checks the positional layout of a tool argument schema, including every
/// variant of a tagged enum.
///
/// # Errors
///
/// Returns an error when a variadic positional field is not the only one or
/// not the last positional field.
pub fn validate_positional_layout(schema: &Value) -> Result<(), PositionalLayoutError> {
    let resolved = resolved_schema(schema, schema);
    if let Some(variants) = resolved.get("oneOf").and_then(Value::as_array) {
        for variant in variants {
            check_object_layout(schema, resolved_schema(schema, variant))?;
        }
    }
    check_object_layout(schema, resolved)
}

fn check_object_layout(root_schema: &Value, object: &Value) -> Result<(), PositionalLayoutError> {
    let (properties, required) = object_fields(object);
    let fields = positional_fields(root_schema, &properties, &required);
    let mut variadic = fields
        .iter()
        .filter(|name| is_array_field(root_schema, &properties, name));
    let Some(first) = variadic.next() else {
        return Ok(());
    };
    if let Some(second) = variadic.next() {
        return Err(PositionalLayoutError::MultipleVariadic {
            first: first.clone(),
            second: second.clone(),
        });
    }
    fields
        .iter()
        .skip_while(|name| *name != first)
        .nth(1)
        .map_or(Ok(()), |after| {
            Err(PositionalLayoutError::VariadicNotLast {
                variadic: first.clone(),
                after: after.clone(),
            })
        })
}

/// The properties and required names of an object schema.
fn object_fields(object: &Value) -> (serde_json::Map<String, Value>, Vec<String>) {
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let required = object
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    (properties, required)
}

fn is_array_field(
    root_schema: &Value,
    properties: &serde_json::Map<String, Value>,
    name: &str,
) -> bool {
    properties
        .get(name)
        .is_some_and(|schema| get_instance_type(root_schema, schema) == Some("array"))
}

/// The fields bare words bind to, in order: the required fields, then the
/// optional fields marked with [`POSITIONAL_KEYWORD`]. An array field joins
/// only when marked, and is then variadic.
fn positional_fields(
    root_schema: &Value,
    properties: &serde_json::Map<String, Value>,
    required: &[String],
) -> Vec<String> {
    let marked = |name: &str| {
        properties
            .get(name)
            .and_then(|schema| schema.get(POSITIONAL_KEYWORD))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    required
        .iter()
        .filter(|name| !is_array_field(root_schema, properties, name) || marked(name))
        .cloned()
        .chain(
            properties
                .keys()
                .filter(|name| !required.contains(name) && marked(name))
                .cloned(),
        )
        .collect()
}

/// Where the next bare word goes.
struct PositionalSlot<'a> {
    field: &'a str,
    /// The type the word is parsed as: the field's, or the item type of a
    /// variadic field.
    value_type: Option<&'a str>,
    variadic: bool,
}

/// The slot the next bare word binds to, if any field is left.
fn next_positional_slot<'a>(
    root_schema: &'a Value,
    schema: &'a ObjectParseSchema,
    state: &ObjectParseState,
) -> Option<PositionalSlot<'a>> {
    let field = schema.positional_fields.get(state.positional_idx)?;
    let prop_schema = schema.properties.get(field)?;
    let variadic = is_array_field(root_schema, &schema.properties, field);
    let value_type = if variadic {
        resolved_schema(root_schema, prop_schema)
            .get("items")
            .and_then(|items| get_instance_type(root_schema, items))
    } else {
        get_instance_type(root_schema, prop_schema)
    };
    Some(PositionalSlot {
        field,
        value_type,
        variadic,
    })
}

/// Whether `arg` is a negative number the next positional field takes.
///
/// Short options are letters, so a number never collides with one unless it
/// carries an exponent letter a declared short option names.
fn is_negative_number_operand(
    root_schema: &Value,
    schema: &ObjectParseSchema,
    state: &ObjectParseState,
    arg: &str,
) -> bool {
    let Some(digits) = arg.strip_prefix('-') else {
        return false;
    };
    if !digits.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        || arg.parse::<f64>().is_err()
        || digits
            .chars()
            .any(|c| schema.short_to_field.contains_key(&c))
    {
        return false;
    }
    next_positional_slot(root_schema, schema, state).is_some_and(|slot| match slot.value_type {
        Some("integer") => arg.parse::<i64>().is_ok(),
        Some("number" | "string") | None => true,
        Some(_) => false,
    })
}

fn parse_positional_arg(
    root_schema: &Value,
    schema: &ObjectParseSchema,
    state: &mut ObjectParseState,
    arg: &str,
) -> anyhow::Result<()> {
    let Some(slot) = next_positional_slot(root_schema, schema, state) else {
        anyhow::bail!("unexpected positional argument: {arg}");
    };
    let value = parse_value(arg, slot.value_type);
    let field = slot.field.to_string();
    if slot.variadic {
        insert_value(
            root_schema,
            &mut state.result,
            &field,
            value,
            &schema.properties[&field],
        );
    } else {
        state.result.insert(field, value);
        state.positional_idx += 1;
    }
    Ok(())
}

fn parse_long_option(
    root_schema: &Value,
    args: &[String],
    i: &mut usize,
    schema: &ObjectParseSchema,
    state: &mut ObjectParseState,
) -> anyhow::Result<OptionOutcome> {
    let arg = &args[*i];
    let flag = &arg[2..];
    if flag == "help" {
        return Ok(OptionOutcome::Help);
    }
    let (mut name, value) = flag.find('=').map_or_else(
        || (flag.to_string(), None),
        |eq_pos| {
            (
                flag[..eq_pos].to_string(),
                Some(flag[eq_pos + 1..].to_string()),
            )
        },
    );
    name = name.replace('_', "-");
    let (normalized_name, negated) = name.strip_prefix("no-").map_or_else(
        || (name.clone(), false),
        |stripped| (stripped.to_string(), true),
    );
    name = normalized_name;
    let field_name = name.replace('-', "_");
    let Some(prop_schema) = schema.properties.get(&field_name) else {
        return Err(unknown_long_option(&name, &schema.long_names));
    };
    let parsed_value =
        parse_long_option_value(root_schema, args, i, &name, value, negated, prop_schema)?;
    insert_value(
        root_schema,
        &mut state.result,
        &field_name,
        parsed_value,
        prop_schema,
    );
    Ok(OptionOutcome::Consumed)
}

fn parse_long_option_value(
    root_schema: &Value,
    args: &[String],
    i: &mut usize,
    name: &str,
    value: Option<String>,
    negated: bool,
    prop_schema: &Value,
) -> anyhow::Result<Value> {
    let prop_type = get_instance_type(root_schema, prop_schema);
    if negated && prop_type != Some("boolean") {
        anyhow::bail!("--no-{name} is only valid for boolean options");
    }
    if prop_type == Some("boolean") {
        if negated && value.is_some() {
            anyhow::bail!("unexpected value for --no-{name}");
        }
        return Ok(value.map_or(Value::Bool(!negated), |v| parse_value(&v, prop_type)));
    }
    let val = value.or_else(|| {
        *i += 1;
        args.get(*i).cloned()
    });
    val.map_or_else(
        || anyhow::bail!("missing value for --{name}"),
        |v| Ok(parse_value(&v, prop_type)),
    )
}

fn unknown_long_option(name: &str, long_names: &[String]) -> anyhow::Error {
    let suggestion = find_similar_option(name, long_names.iter().map(String::as_str));
    suggestion.map_or_else(
        || anyhow::anyhow!("unknown option: --{name}"),
        |similar| anyhow::anyhow!("unknown option: --{name}. Did you mean --{similar}?"),
    )
}

fn parse_short_options(
    root_schema: &Value,
    arg: &str,
    args: &[String],
    i: &mut usize,
    properties: &serde_json::Map<String, Value>,
    short_to_field: &HashMap<char, String>,
    result: &mut HashMap<String, Value>,
) -> anyhow::Result<OptionOutcome> {
    let mut cluster = &arg[1..];
    let mut value_from_eq = None;
    if let Some(eq_pos) = cluster.find('=') {
        value_from_eq = Some(cluster[eq_pos + 1..].to_string());
        cluster = &cluster[..eq_pos];
    }

    let mut chars = cluster.chars().peekable();
    while let Some(ch) = chars.next() {
        // `h` is reserved: no field is given it as a short option.
        if ch == 'h' {
            return Ok(OptionOutcome::Help);
        }
        let field_name = short_to_field
            .get(&ch)
            .ok_or_else(|| anyhow::anyhow!("unknown option: -{ch}"))?
            .clone();
        let prop_schema = properties
            .get(&field_name)
            .ok_or_else(|| anyhow::anyhow!("unknown option: -{ch}"))?;
        let prop_type = get_instance_type(root_schema, prop_schema);

        if prop_type == Some("boolean") {
            if chars.peek().is_none() {
                if let Some(v) = value_from_eq.take() {
                    let parsed = parse_value(&v, prop_type);
                    insert_value(root_schema, result, &field_name, parsed, prop_schema);
                } else {
                    insert_value(
                        root_schema,
                        result,
                        &field_name,
                        Value::Bool(true),
                        prop_schema,
                    );
                }
            } else {
                if value_from_eq.is_some() {
                    anyhow::bail!("unexpected value for -{ch}");
                }
                insert_value(
                    root_schema,
                    result,
                    &field_name,
                    Value::Bool(true),
                    prop_schema,
                );
            }
            continue;
        }

        let value = if let Some(v) = value_from_eq.take() {
            if chars.peek().is_some() {
                anyhow::bail!("unexpected value for -{ch}");
            }
            v
        } else if chars.peek().is_some() {
            let rest: String = chars.collect();
            rest
        } else {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing value for -{ch}"))?
        };

        let parsed = parse_value(&value, prop_type);
        insert_value(root_schema, result, &field_name, parsed, prop_schema);
        break;
    }

    Ok(OptionOutcome::Consumed)
}

fn insert_value(
    root_schema: &Value,
    result: &mut HashMap<String, Value>,
    field_name: &str,
    value: Value,
    prop_schema: &Value,
) {
    if get_instance_type(root_schema, prop_schema) == Some("array") {
        if let Some(Value::Array(existing)) = result.get_mut(field_name) {
            match value {
                Value::Array(mut items) => existing.append(&mut items),
                other => existing.push(other),
            }
        } else {
            let mut items = Vec::new();
            match value {
                Value::Array(mut arr) => items.append(&mut arr),
                other => items.push(other),
            }
            result.insert(field_name.to_string(), Value::Array(items));
        }
    } else {
        result.insert(field_name.to_string(), value);
    }
}

/// Gets the instance type from a schema.
/// Handles both simple types and Option<T>.
fn get_instance_type<'a>(root_schema: &'a Value, schema: &'a Value) -> Option<&'a str> {
    let schema = resolved_schema(root_schema, schema);
    match schema.get("type") {
        // Direct string type: "type": "integer"
        Some(Value::String(t)) => Some(t.as_str()),

        // Array of types (schemars 1.0 for Option<T>): "type": ["integer", "null"]
        Some(Value::Array(types)) => {
            // Find the non-null type
            for t in types {
                if let Some(s) = t.as_str()
                    && s != "null"
                {
                    return Some(s);
                }
            }
            None
        }

        // No direct type - try anyOf/oneOf (older schemars or complex types)
        None => {
            let variants = schema
                .get("anyOf")
                .or_else(|| schema.get("oneOf"))
                .and_then(Value::as_array)?;

            for variant in variants {
                if let Some(t) = variant.get("type").and_then(Value::as_str)
                    && t != "null"
                {
                    return Some(t);
                }
            }
            None
        }

        _ => None,
    }
}

fn resolved_schema<'a>(root_schema: &'a Value, schema: &'a Value) -> &'a Value {
    let mut current = schema;
    while let Some(reference) = current.get("$ref").and_then(Value::as_str) {
        let Some(next) = resolve_local_ref(root_schema, reference) else {
            break;
        };
        if std::ptr::eq(current, next) {
            break;
        }
        current = next;
    }
    current
}

fn resolve_local_ref<'a>(root_schema: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix("#/")?;
    let mut current = root_schema;
    for segment in pointer.split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        current = current.get(segment.as_str())?;
    }
    Some(current)
}

/// Parses a string value according to expected type.
fn parse_value(s: &str, expected_type: Option<&str>) -> Value {
    match expected_type {
        Some("integer") => s
            .parse::<i64>()
            .map_or_else(|_| Value::String(s.to_string()), Value::from),
        Some("number") => s
            .parse::<f64>()
            .map_or_else(|_| Value::String(s.to_string()), Value::from),
        Some("boolean") => match s.to_lowercase().as_str() {
            "true" | "1" | "yes" => Value::Bool(true),
            "false" | "0" | "no" => Value::Bool(false),
            _ => Value::String(s.to_string()),
        },
        Some("array") => {
            // Try parsing as JSON array, fallback to single-element array
            serde_json::from_str(s)
                .unwrap_or_else(|_| Value::Array(vec![Value::String(s.to_string())]))
        }
        Some("object") => {
            // Try parsing as JSON object
            serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
        }
        _ => Value::String(s.to_string()),
    }
}

/// Generates help text from a JSON schema.
#[must_use]
pub fn schema_to_help(schema: &Value) -> String {
    let mut help = String::new();

    // The schema's title is the argument struct's Rust type name, which is
    // no part of the command line; only the description is shown.
    if let Some(desc) = schema.get("description").and_then(Value::as_str) {
        help.push_str(desc);
        help.push('\n');
    }

    help.push_str("\nUsage:\n");

    // Check for tagged enum (subcommands)
    if let Some(variants) = schema.get("oneOf").and_then(Value::as_array)
        && let Some(tag) = find_serde_tag(schema)
    {
        help.push_str("  <subcommand> [options]\n\n");
        help.push_str("Subcommands:\n");

        for variant in variants {
            if let Some(name) = get_variant_name(variant, &tag) {
                help.push_str("  ");
                help.push_str(name.as_str());
                if let Some(desc) = variant.get("description").and_then(Value::as_str) {
                    help.push_str(" - ");
                    help.push_str(desc);
                }
                help.push('\n');
            }
        }
        help.push_str("\nOptions:\n  -h, --help  Show help\n");
        return help;
    }

    // Simple object
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        // Show positional usage: the fields bare words bind to, optional
        // ones in brackets.
        let required_names: Vec<String> = required.iter().map(|n| (*n).to_string()).collect();
        let positional_names = positional_fields(schema, props, &required_names);
        let positional: Vec<_> = positional_names
            .iter()
            .map(|name| {
                render_positional_usage(
                    name,
                    required.contains(&name.as_str()),
                    is_array_field(schema, props, name),
                )
            })
            .collect();
        // Show required array fields that are not positional as repeatable
        // flags in usage
        let repeatable: Vec<_> = required
            .iter()
            .filter(|n| {
                is_array_field(schema, props, n) && !positional_names.iter().any(|p| p == **n)
            })
            .map(|n| render_repeatable_usage(n))
            .collect();

        let mut usage_parts = Vec::new();
        if !positional.is_empty() {
            usage_parts.push(positional.join(" "));
        }
        for r in &repeatable {
            usage_parts.push(r.clone());
        }
        usage_parts.push("[options]".to_string());
        let usage = usage_parts.join(" ");
        help.push_str("  ");
        push_help_line(&mut help, usage.as_str());

        help.push_str("\nOptions:\n  -h, --help  Show help\n");
        help.push_str("\nArguments:\n");

        let (_, field_to_short) = build_short_option_maps(props);

        for (name, prop) in props {
            let is_required = required.contains(&name.as_str());
            let is_array = get_instance_type(prop, prop) == Some("array");
            let flag = name.replace('_', "-");

            if positional_names.iter().any(|positional| positional == name) {
                // A positional argument is given as a bare word, never as a flag.
                help.push_str("  <");
                help.push_str(flag.as_str());
                help.push('>');
                if is_array {
                    help.push_str("...");
                }
                if is_required {
                    help.push_str(" (required)");
                }
            } else {
                let short = field_to_short.get(name).copied();
                help.push_str(render_flag_prefix(flag.as_str(), short).as_str());
                if is_array {
                    help.push_str(" <value>  (repeatable");
                    if is_required {
                        help.push_str(", required");
                    }
                    help.push(')');
                } else if is_required {
                    help.push_str(" (required)");
                }
            }

            if let Some(desc) = prop.get("description").and_then(Value::as_str) {
                help.push_str("\n      ");
                help.push_str(desc);
            }
            help.push('\n');
        }
    }

    help
}

/// Detects the stdin argument from a JSON schema.
///
/// Returns "input" if the schema has an "input" property that is not required.
/// This allows stdin piping: `cat file | command "prompt"` passes stdin as --input.
pub(super) fn detect_stdin_arg(schema: &Value) -> Option<String> {
    // Check if "input" property exists
    let properties = schema.get("properties")?.as_object()?;
    if !properties.contains_key("input") {
        return None;
    }

    // Check that "input" is NOT required (has a default)
    let required = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();

    if required.contains(&"input") {
        return None;
    }

    Some("input".to_string())
}

/// Rebuilds the command line a `heel ipc` request carries.
///
/// Raw-args commands deliver their argv untouched in `args`; named entries —
/// the declared stdin field, or named parameters from a bound-args client —
/// are appended as `--name value` pairs, unless the argv already declared
/// them (an explicit flag on the command line wins over piped input).
pub fn flatten_args_to_cli(args: &std::collections::HashMap<String, Value>) -> Vec<String> {
    let mut cli_args = Vec::new();
    if let Some(Value::Array(arr)) = args.get("args") {
        cli_args.extend(arr.iter().filter_map(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }));
    }

    // Bound fields are options, so they belong before the option terminator;
    // everything from the terminator on is positional.
    let terminator = cli_args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(cli_args.len());
    let positionals = cli_args.split_off(terminator);

    // Options already spelled out in the verbatim argv keep their value; a
    // same-named bound field would only come from piped stdin.
    let declared: HashSet<String> = cli_args
        .iter()
        .filter_map(|arg| arg.strip_prefix("--"))
        .filter_map(|name| name.split('=').next())
        .map(str::to_string)
        .collect();

    let mut keys = args
        .keys()
        .map(String::as_str)
        .filter(|key| *key != "args")
        .collect::<Vec<_>>();
    keys.sort_unstable();
    for key in keys {
        if declared.contains(key.replace('_', "-").as_str()) {
            continue;
        }
        let value = args
            .get(key)
            .expect("flatten_args_to_cli keys must resolve back into the map");
        flatten_named_arg_to_cli(&mut cli_args, key, value);
    }
    cli_args.extend(positionals);
    cli_args
}

fn push_long_option(cli_args: &mut Vec<String>, key: &str) {
    let mut option = String::with_capacity(key.len() + 2);
    option.push_str("--");
    option.push_str(key);
    cli_args.push(option);
}

fn flatten_named_arg_to_cli(cli_args: &mut Vec<String>, key: &str, value: &Value) {
    match value {
        Value::Bool(true) => push_long_option(cli_args, key),
        Value::Bool(false) => {}
        Value::String(text) => {
            push_long_option(cli_args, key);
            cli_args.push(text.clone());
        }
        Value::Number(number) => {
            push_long_option(cli_args, key);
            cli_args.push(number.to_string());
        }
        Value::Array(items) => {
            for item in items {
                push_long_option(cli_args, key);
                if let Some(text) = item.as_str() {
                    cli_args.push(text.to_string());
                } else {
                    cli_args.push(item.to_string());
                }
            }
        }
        _ => {
            push_long_option(cli_args, key);
            cli_args.push(value.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::Value;

    use super::{flatten_args_to_cli, requests_help};

    fn bound(argv: &[&str], input: &str) -> std::collections::HashMap<String, Value> {
        std::collections::HashMap::from([
            ("args".to_string(), serde_json::json!(argv)),
            ("input".to_string(), Value::String(input.to_string())),
        ])
    }

    #[test]
    fn piped_stdin_is_bound_before_the_option_terminator() {
        assert_eq!(
            flatten_args_to_cli(&bound(&["--", "word"], "x")),
            words(&["--input", "x", "--", "word"])
        );
        assert_eq!(
            flatten_args_to_cli(&bound(&["--verbose"], "x")),
            words(&["--verbose", "--input", "x"])
        );
    }

    #[test]
    fn a_literal_option_after_the_terminator_is_not_declared() {
        assert_eq!(
            flatten_args_to_cli(&bound(&["--", "--input"], "x")),
            words(&["--input", "x", "--", "--input"])
        );
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Piped {
        /// The first word.
        prompt: String,
        /// Piped stdin.
        #[serde(default)]
        input: Option<String>,
    }

    /// What the command receives once the rebuilt line is parsed.
    fn parsed(argv: &[&str], input: &str) -> Value {
        let schema = serde_json::to_value(schemars::schema_for!(Piped)).expect("a schema");
        super::cli_to_json(&schema, &flatten_args_to_cli(&bound(argv, input))).expect("parses")
    }

    #[test]
    fn piped_stdin_reaches_its_field_through_the_terminator() {
        assert_eq!(
            parsed(&["--", "word"], "x"),
            serde_json::json!({ "prompt": "word", "input": "x" })
        );
        assert_eq!(
            parsed(&["--", "--input"], "x"),
            serde_json::json!({ "prompt": "--input", "input": "x" })
        );
    }

    #[test]
    fn an_option_before_the_terminator_still_wins_over_stdin() {
        assert_eq!(
            flatten_args_to_cli(&bound(&["--input", "a", "--", "word"], "x")),
            words(&["--input", "a", "--", "word"])
        );
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Args {
        /// The first word.
        prompt: String,
        /// A body.
        #[serde(default)]
        body: Option<String>,
        /// A switch.
        #[serde(default)]
        verbose: bool,
    }

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(|word| (*word).to_string()).collect()
    }

    fn help_in(line: &[&str]) -> bool {
        let schema = serde_json::to_value(schemars::schema_for!(Args)).expect("a schema");
        requests_help(Some(&schema), &words(line))
    }

    #[test]
    fn help_flags_in_option_position_request_help() {
        assert!(help_in(&["-h"]));
        assert!(help_in(&["--help"]));
        assert!(help_in(&["word", "--verbose", "--help"]));
        assert!(help_in(&["-vh"]));
    }

    #[test]
    fn an_option_value_is_never_a_help_request() {
        assert!(!help_in(&["word", "--body", "- fixed the crash"]));
        assert!(!help_in(&["word", "--body=-h"]));
        assert!(!help_in(&["word", "-b", "--help"]));
        assert!(!help_in(&["word", "-b-h"]));
    }

    #[test]
    fn words_after_the_terminator_are_not_flags() {
        assert!(!help_in(&["--", "-h"]));
        assert!(!help_in(&["word", "--", "--help"]));
    }

    #[test]
    fn help_is_read_before_required_arguments_are_checked() {
        // `prompt` is required and absent.
        assert!(help_in(&["--help"]));
        assert!(help_in(&["--verbose", "-h"]));
        assert!(!help_in(&["--verbose"]));
    }

    #[test]
    fn usage_shows_no_type_name_and_no_flag_for_a_positional() {
        let schema = serde_json::to_value(schemars::schema_for!(Args)).expect("a schema");
        let help = super::schema_to_help(&schema);
        assert!(!help.contains("Args"), "{help}");
        assert!(help.contains("  <prompt> [options]"), "{help}");
        assert!(help.contains("  <prompt> (required)"), "{help}");
        assert!(!help.contains("--prompt"), "{help}");
        assert!(help.contains("--body"), "{help}");
        assert!(help.contains("--verbose"), "{help}");
    }

    #[test]
    fn a_raw_command_takes_only_the_bare_flags() {
        assert!(requests_help(None, &words(&["--help"])));
        assert!(!requests_help(None, &words(&["--", "-h"])));
        assert!(!requests_help(None, &words(&["- the notes"])));
    }

    #[test]
    fn a_tagged_enum_reads_help_after_its_subcommand() {
        #[derive(Debug, Deserialize, JsonSchema)]
        #[serde(tag = "operation", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum Op {
            Comment { number: u64, body: String },
            Close { number: u64 },
        }
        let schema: Value = serde_json::to_value(schemars::schema_for!(Op)).expect("a schema");
        let help = |line: &[&str]| requests_help(Some(&schema), &words(line));
        assert!(help(&["--help"]));
        assert!(help(&["comment", "--help"]));
        assert!(!help(&["comment", "12", "--body", "- fixed the crash"]));
        assert!(!help(&["comment", "12", "-b", "- the thing"]));
    }
}
