//! Characterization pins for YAML decoding: how scalars resolve, how decoded
//! values render in templates and through the emitter, and the wording of
//! decode errors. Expectations are what serde_yaml_ng 0.10 produces today,
//! odd cases included: decoded values feed fingerprint checksums and template
//! output, so a YAML backend swap must reproduce them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::HashSet;

use taskcore::ast::{Dialect, Taskfile};
use taskcore::reader::{Node, Reader, ReaderError};
use taskcore::templater::Cache;
use taskcore::yaml::{Mapping, Number, Tag, TaggedValue, Value, from_str, to_string};

/// A backend-independent rendering of a value: the variant, then the number's
/// `Display`, the tag, or the string's `Debug` form.
fn describe(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => format!("bool:{b}"),
        Value::Number(n) => format!("num:{n}"),
        Value::String(s) => format!("str:{s:?}"),
        Value::Sequence(seq) => {
            let items: Vec<String> = seq.iter().map(describe).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Mapping(map) => {
            let entries: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{} => {}", describe(k), describe(v)))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
        Value::Tagged(t) => format!("tagged:{}:{}", t.tag, describe(&t.value)),
    }
}

/// The value of `v` in the single-document `yaml`.
fn parse_v(yaml: &str) -> Result<String, String> {
    let doc: Value = from_str(yaml).map_err(|e| e.to_string())?;
    let Value::Mapping(map) = &doc else {
        panic!("not a mapping: {yaml:?}");
    };
    Ok(describe(map.get("v").expect("no `v` key")))
}

/// The static value of `V` in a Taskfile's top-level `vars:`.
fn taskfile_var(text: &str) -> Result<String, String> {
    let yaml = format!("version: '3'\nvars:\n  V: {text}\n");
    let tf: Taskfile = from_str(&yaml).map_err(|e| e.to_string())?;
    let var = tf.vars.get("V").expect("no V");
    Ok(describe(var.value.as_ref().expect("no static value")))
}

/// Plain and tagged scalars → resolved value. serde_yaml_ng follows neither
/// YAML 1.1 nor 1.2 exactly: only true/false/null (in three casings) and `~`
/// are special, no timestamps or sexagesimals, `0x`/`0o`/`0b` need a
/// lowercase prefix, and a leading zero keeps a string.
const SCALARS: &[(&str, &str)] = &[
    // YAML 1.1 booleans and nulls: only the 1.2 core spellings resolve.
    ("yes", r#"str:"yes""#),
    ("no", r#"str:"no""#),
    ("on", r#"str:"on""#),
    ("off", r#"str:"off""#),
    ("y", r#"str:"y""#),
    ("n", r#"str:"n""#),
    ("Y", r#"str:"Y""#),
    ("NO", r#"str:"NO""#),
    ("Off", r#"str:"Off""#),
    ("true", "bool:true"),
    ("True", "bool:true"),
    ("TRUE", "bool:true"),
    ("tRUE", r#"str:"tRUE""#),
    ("False", "bool:false"),
    ("FALSE", "bool:false"),
    ("null", "null"),
    ("Null", "null"),
    ("NULL", "null"),
    ("~", "null"),
    ("", "null"),
    // Leading `+` and signed zero. `-0` is an integer, so the sign is lost.
    ("+1", "num:1"),
    ("+0", "num:0"),
    ("+1.5", "num:1.5"),
    ("+.inf", "num:.inf"),
    ("-0", "num:0"),
    ("-0.0", "num:-0.0"),
    ("+-1", r#"str:"+-1""#),
    // YAML 1.1 timestamps and sexagesimals stay strings.
    ("2001-12-14", r#"str:"2001-12-14""#),
    (
        "2001-12-14t21:59:43.10-05:00",
        r#"str:"2001-12-14t21:59:43.10-05:00""#,
    ),
    ("1:20", r#"str:"1:20""#),
    ("190:20:30", r#"str:"190:20:30""#),
    // Floats. Display keeps a `.0` on integral floats; 3.10 loses its zero.
    (".5", "num:0.5"),
    ("-.5", "num:-0.5"),
    ("5.", "num:5.0"),
    ("0.", "num:0.0"),
    ("1E3", "num:1000.0"),
    ("1e+3", "num:1000.0"),
    ("1.0e-3", "num:0.001"),
    ("0.1", "num:0.1"),
    ("3.10", "num:3.1"),
    ("1e400", r#"str:"1e400""#), // overflows f64: kept as a string
    (".inf", "num:.inf"),
    (".INF", "num:.inf"),
    (".Inf", "num:.inf"),
    ("-.INF", "num:-.inf"),
    (".nan", "num:.nan"),
    (".NaN", "num:.nan"),
    ("-.nan", r#"str:"-.nan""#),
    // Integers.
    ("0x1F", "num:31"),
    ("-0x1F", "num:-31"),
    ("+0x1F", "num:31"),
    ("-0x0", "num:0"),
    ("0X1F", r#"str:"0X1F""#),
    ("0x", r#"str:"0x""#),
    ("0x_1F", r#"str:"0x_1F""#),
    ("0b1", "num:1"),
    ("0b", r#"str:"0b""#),
    ("0o17", "num:15"),
    ("0o8", r#"str:"0o8""#),
    ("0o", r#"str:"0o""#),
    ("017", r#"str:"017""#), // neither octal nor decimal
    ("1_0", r#"str:"1_0""#),
    ("18446744073709551615", "num:18446744073709551615"),
    ("-9223372036854775808", "num:-9223372036854775808"),
    // Core and custom tags. `!!float`/`!!int` re-resolve the scalar; other
    // `!!` tags vanish; a local tag is kept around the resolved value.
    ("!!float 1", "num:1.0"),
    ("!!float '1'", "num:1.0"),
    ("!!int 0x1F", "num:31"),
    ("!!bool true", "bool:true"),
    ("!!null ~", "null"),
    ("!!str 1", r#"str:"1""#),
    ("!!binary aGk=", r#"str:"aGk=""#),            // not decoded
    ("!<tag:example.com,2000:x> 5", r#"str:"5""#), // global tag dropped, scalar unresolved
    ("!custom x", r#"tagged:!custom:str:"x""#),
    ("!custom 1", "tagged:!custom:num:1"),
    // Quoted scalars never resolve; escapes are decoded.
    ("'0x1F'", r#"str:"0x1F""#),
    ("'it''s'", r#"str:"it's""#),
    (r#""\x41é\N""#, r#"str:"Aé\u{85}""#),
];

#[test]
fn scalars_resolve_as_pinned() {
    for (text, want) in SCALARS {
        assert_eq!(
            parse_v(&format!("v: {text}\n")).as_deref(),
            Ok(*want),
            "v: {text}"
        );
        // A Taskfile variable keeps the scalar as decoded.
        assert_eq!(taskfile_var(text).as_deref(), Ok(*want), "V: {text}");
    }
}

#[test]
fn rejected_scalars_are_pinned() {
    // Out-of-range integers are errors rather than strings or floats, and the
    // narrow core tags reject anything outside their core spellings instead of
    // re-resolving it.
    let cases = [
        (
            "18446744073709551616",
            "invalid type: integer `18446744073709551616` as u128, expected any YAML value",
        ),
        (
            "-9223372036854775809",
            "invalid type: integer `-9223372036854775809` as i128, expected any YAML value",
        ),
        (
            "!!bool yes",
            "invalid value: string \"yes\", expected a boolean",
        ),
        ("!!null ''", "invalid value: string \"\", expected null"),
        (
            "!!int 017",
            "invalid value: string \"017\", expected an integer",
        ),
    ];
    for (text, msg) in cases {
        assert_eq!(
            parse_v(&format!("v: {text}\n")),
            Err(format!("v: {msg} at line 1 column 4")),
            "{text}"
        );
        assert_eq!(
            taskfile_var(text),
            Err(format!(
                "task: Failed to decode Taskfile: vars.V: {msg} at line 3 column 6"
            )),
            "{text}"
        );
    }
}

/// Collections, merges, block scalars and keys → value (or error message).
const DOCUMENTS: &[(&str, Result<&str, &str>)] = &[
    // Merge keys are not applied: `<<` stays an ordinary string key.
    (
        "base: &base {a: 1, b: 2}\nv:\n  <<: *base\n  b: 3\n",
        Ok(r#"{str:"<<" => {str:"a" => num:1, str:"b" => num:2}, str:"b" => num:3}"#),
    ),
    (
        "a: &a {x: 1}\nb: &b {y: 2}\nv:\n  <<: [*a, *b]\n  w: 0\n",
        Ok(r#"{str:"<<" => [{str:"x" => num:1}, {str:"y" => num:2}], str:"w" => num:0}"#),
    ),
    ("a: &x 007\nv: *x\n", Ok(r#"str:"007""#)),
    // Explicit core collection tags vanish; local tags wrap the collection.
    ("v: !!map {a: 1}\n", Ok(r#"{str:"a" => num:1}"#)),
    ("v: !!seq [1, 2]\n", Ok("[num:1, num:2]")),
    ("v: !tag {a: 1}\n", Ok(r#"tagged:!tag:{str:"a" => num:1}"#)),
    ("v: !tag [1]\n", Ok("tagged:!tag:[num:1]")),
    // Block scalars and folding.
    (
        "v: >\n  folded\n  text\n\n  para\n",
        Ok(r#"str:"folded text\npara\n""#),
    ),
    ("v: >-\n  a\n  b\n", Ok(r#"str:"a b""#)),
    ("v: |\n  a\n  b\n", Ok(r#"str:"a\nb\n""#)),
    ("v: |-\n  keep\n  lines\n\n", Ok(r#"str:"keep\nlines""#)),
    (
        "v: |+\n  keep\n  trail\n\n\nw: 1\n",
        Ok(r#"str:"keep\ntrail\n\n\n""#),
    ),
    (
        "v: plain\n  multi\n  line\n\n  para\n",
        Ok(r#"str:"plain multi line\npara""#),
    ),
    ("v: \"a\\\n  b\"\n", Ok(r#"str:"ab""#)),
    // Non-string keys are typed like values.
    (
        "v: {[1]: a, {b: 1}: c, 1.5: d, !t k: e, true: f, ~: g, 0x10: h}\n",
        Ok(
            r#"{[num:1] => str:"a", {str:"b" => num:1} => str:"c", num:1.5 => str:"d", tagged:!t:str:"k" => str:"e", bool:true => str:"f", null => str:"g", num:16 => str:"h"}"#,
        ),
    ),
    // Duplicate detection compares resolved keys, except that integers and
    // floats never compare equal and a string never equals a number.
    (
        "v: {16: a, 0x10: b}\n",
        Err("v: duplicate entry with key 16 at line 1 column 4"),
    ),
    (
        "v: {0o17: a, 15: b}\n",
        Err("v: duplicate entry with key 15 at line 1 column 4"),
    ),
    (
        "v: {true: a, True: b}\n",
        Err("v: duplicate entry with key `true` at line 1 column 4"),
    ),
    (
        "v: {~: a, null: b}\n",
        Err("v: duplicate entry with null key at line 1 column 4"),
    ),
    (
        "v: {.inf: a, .Inf: b}\n",
        Err("v: duplicate entry with key .inf at line 1 column 4"),
    ),
    (
        "v: {a: 1, a: 2}\n",
        Err("v: duplicate entry with key \"a\" at line 1 column 4"),
    ),
    (
        "v: {1: a, 1.0: b}\n",
        Ok(r#"{num:1 => str:"a", num:1.0 => str:"b"}"#),
    ),
    (
        "v: {1: a, '1': b}\n",
        Ok(r#"{num:1 => str:"a", str:"1" => str:"b"}"#),
    ),
];

#[test]
fn documents_decode_as_pinned() {
    for (yaml, want) in DOCUMENTS {
        let got = parse_v(yaml);
        assert_eq!(got.as_deref().map_err(String::as_str), *want, "{yaml:?}");
    }
}

#[test]
fn taskfile_level_resolution_is_pinned() {
    let decode = |yaml: &str| from_str::<Taskfile>(yaml).map_err(|e| e.to_string());

    // `version` is read through a float, so the trailing zero is lost.
    assert_eq!(
        decode("version: 3.10\n").unwrap().version.as_deref(),
        Some("3.1")
    );
    assert_eq!(
        decode("version: 3.0\n").unwrap().version.as_deref(),
        Some("3.0")
    );

    // Non-string keys under `vars:` are rejected; the decode prefix repeats
    // once per nested Deserialize impl that wraps the error.
    for yaml in [
        "version: '3'\nvars:\n  1: one\n",
        "version: '3'\nvars:\n  true: t\n",
        "version: '3'\nvars:\n  ~: n\n",
        "version: '3'\nvars:\n  [1]: a\n",
        "version: '3'\nvars:\n  1: a\n  1.0: b\n",
    ] {
        assert_eq!(
            decode(yaml).unwrap_err(),
            "task: Failed to decode Taskfile: task: Failed to decode Taskfile: cannot unmarshal into vars",
            "{yaml:?}"
        );
    }
    // A duplicate is detected before the keys are type-checked.
    assert_eq!(
        decode("version: '3'\nvars:\n  16: a\n  0x10: b\n").unwrap_err(),
        "task: Failed to decode Taskfile: vars: duplicate entry with key 16 at line 3 column 3"
    );
    // A merge key inside a `map:` variable is kept literally.
    let tf = decode("version: '3'\nvars:\n  X: {map: {<<: {a: 1}, b: 2}}\n").unwrap();
    assert_eq!(
        describe(tf.vars.get("X").unwrap().value.as_ref().unwrap()),
        r#"{str:"<<" => {str:"a" => num:1}, str:"b" => num:2}"#
    );
}

/// Template output of decoded values. Both dialects render the same text: a
/// tagged value renders as a one-entry map, null as `None`.
#[test]
fn decoded_values_render_as_pinned() {
    // (variable, YAML value, rendered text)
    let cases = [
        ("OCT", "017", "017"),
        ("OCTO", "0o17", "15"),
        ("HEX", "0x1F", "31"),
        ("HUGE", "18446744073709551615", "18446744073709551615"),
        ("NEG", "-9223372036854775808", "-9223372036854775808"),
        ("BIG", "1e20", "100000000000000000000.0"),
        ("TAG", "!foo bar", r#"{"!foo": "bar"}"#),
        ("TAGN", "!foo 5", r#"{"!foo": 5}"#),
        ("STR", "!!str 1", "1"),
        ("YES", "yes", "yes"),
        ("TRU", "True", "true"),
        ("FLT", "1.0", "1.0"),
        ("F3", "3.10", "3.1"),
        ("EXP", "1e3", "1000.0"),
        ("INF", ".inf", "inf"),
        ("NAN", ".nan", "NaN"),
        ("NZ", "-0.0", "-0.0"),
        ("NUL", "~", "None"),
        ("SEQ", "[1, yes, ~, 1.0]", r#"[1, "yes", None, 1.0]"#),
        ("MAP", "{map: {a: 1, b: [x]}}", r#"{"a": 1, "b": ["x"]}"#),
    ];
    let mut yaml = String::from("version: '3'\nvars:\n");
    for (name, value, _) in cases {
        yaml.push_str(&format!("  {name}: {value}\n"));
    }
    let tf: Taskfile = from_str(&yaml).unwrap();
    for dialect in [Dialect::Go, Dialect::Jinja] {
        let mut cache = Cache::new(tf.vars.clone());
        cache.set_dialect(dialect);
        for (name, _, text) in cases {
            let tmpl = match dialect {
                Dialect::Go => format!("{{{{.{name}}}}}"),
                Dialect::Jinja => format!("{{{{ {name} }}}}"),
            };
            assert_eq!(cache.replace(&tmpl), text, "{dialect:?} {name}");
        }
        assert!(!cache.is_err(), "{:?}", cache.err());
    }
}

/// Emitter output for the values callers stringify (non-string map keys in
/// `for:` loops, non-scalar values in summaries).
#[test]
fn emitter_output_is_pinned() {
    let cases = [
        ("1", "1\n"),
        ("1.0", "1.0\n"),
        ("-0.0", "-0.0\n"),
        (".inf", ".inf\n"),
        (".nan", ".nan\n"),
        ("18446744073709551615", "18446744073709551615\n"),
        ("true", "true\n"),
        ("~", "null\n"),
        // A string that would re-resolve as another type is quoted; YAML 1.1
        // spellings are not, since they read back as strings here.
        ("'1'", "'1'\n"),
        ("'017'", "'017'\n"),
        ("'0x1F'", "'0x1F'\n"),
        ("'1e3'", "'1e3'\n"),
        ("'.inf'", "'.inf'\n"),
        ("'~'", "'~'\n"),
        ("'null'", "'null'\n"),
        ("'yes'", "yes\n"),
        ("'on'", "on\n"),
        ("'Y'", "Y\n"),
        ("'2001-12-14'", "2001-12-14\n"),
        ("''", "''\n"),
        ("'a: b'", "'a: b'\n"),
        ("' lead'", "' lead'\n"),
        ("'#x'", "'#x'\n"),
        ("'- x'", "'- x'\n"),
        ("'é'", "é\n"),
        (r#""tab\there""#, "\"tab\\there\"\n"),
        (r#""\x85""#, "\"\\N\"\n"),
        (r#""x\ny""#, "|-\n  x\n  y\n"),
        (r#""x\ny\n""#, "|\n  x\n  y\n"),
        ("!foo bar", "!foo bar\n"),
        ("!foo [1]", "!foo\n- 1\n"),
        ("[1, a]", "- 1\n- a\n"),
        (
            "{1: a, true: b, ~: c, [1]: d, {k: v}: e}",
            "1: a\ntrue: b\nnull: c\n? - 1\n: d\n? k: v\n: e\n",
        ),
    ];
    for (yaml, want) in cases {
        let value: Value = from_str(yaml).unwrap();
        assert_eq!(to_string(&value).unwrap(), want, "{yaml}");
    }
}

/// The parts of the value model callers construct or match on directly.
#[test]
fn value_model_api_is_pinned() {
    assert_eq!(Number::from(42).to_string(), "42");
    assert_eq!(Value::Number(42.into()), from_str::<Value>("42").unwrap());

    // A mapping keeps insertion order; `insert` replaces in place.
    let mut map = Mapping::new();
    map.insert(Value::String("b".into()), Value::Bool(true));
    map.insert(Value::Number(1.into()), Value::Null);
    assert_eq!(
        map.insert(Value::String("b".into()), Value::Bool(false)),
        Some(Value::Bool(true))
    );
    assert_eq!(
        describe(&Value::Mapping(map.clone())),
        r#"{str:"b" => bool:false, num:1 => null}"#
    );
    assert_eq!(map.get("b"), Some(&Value::Bool(false)));

    // Values hash and compare structurally, usable as set members.
    let set: HashSet<Value> = from_str::<Vec<Value>>("[1, '1', 1.0, 1, {a: 1}, {a: 1}]")
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(set.len(), 4);

    // Tags compare without their leading `!` and display with it.
    let tagged = Value::Tagged(Box::new(TaggedValue {
        tag: Tag::new("foo"),
        value: Value::String("bar".into()),
    }));
    assert_eq!(from_str::<Value>("!foo bar").unwrap(), tagged);
    assert_eq!(Tag::new("foo").to_string(), "!foo");
}

/// An in-memory Taskfile for the reader.
struct Source(&'static str);

impl Node for Source {
    fn read(&self) -> Result<Vec<u8>, ReaderError> {
        Ok(self.0.as_bytes().to_vec())
    }
    fn location(&self) -> &str {
        "Taskfile.yml"
    }
    fn dir(&self) -> &str {
        "."
    }
    fn resolve_entrypoint(&self, entrypoint: &str) -> Result<String, ReaderError> {
        Ok(entrypoint.to_string())
    }
    fn resolve_dir(&self, dir: &str) -> Result<String, ReaderError> {
        Ok(dir.to_string())
    }
}

fn read_error(src: &'static str) -> String {
    Reader::new().read(&Source(src)).unwrap_err().to_string()
}

const SYNTAX_ERROR: &str = "version: '3'\ntasks:\n  a: [1\n";

// Every Taskfile decode error reaches the reader wrapped by the Taskfile's
// Deserialize impl, which drops its location: the snippet branch in
// `Reader::read_node` never runs, and the position shows only in the text.
#[test]
fn syntax_error_position_is_pinned() {
    let cases = [
        // An unclosed flow sequence is reported where the parser gives up
        // (the next line), not at the opening bracket.
        (SYNTAX_ERROR, (4, 1)),
        (
            "version: '3'\ntasks:\n  a:\n    cmds:\n      - echo\n    deps: x: y\n",
            (6, 12),
        ),
    ];
    for (src, (line, column)) in cases {
        let err = from_str::<Value>(src).unwrap_err();
        let loc = err.location().unwrap();
        assert_eq!((loc.line(), loc.column()), (line, column), "{src:?}");

        let msg = read_error(src);
        assert!(
            msg.starts_with(
                "task: Failed to parse Taskfile.yml:\ntask: Failed to decode Taskfile: "
            ),
            "{msg}"
        );
        assert!(
            msg.contains(&format!(" at line {line} column {column}")),
            "{msg}"
        );
        assert_eq!(msg.lines().count(), 2, "unexpected snippet: {msg}");
    }
}

// The wording comes from the YAML parser and is expected to change with it.
#[test]
fn syntax_error_wording_is_libyamls() {
    assert_eq!(
        read_error(SYNTAX_ERROR),
        "task: Failed to parse Taskfile.yml:\ntask: Failed to decode Taskfile: did not find expected ',' or ']' at line 4 column 1, while parsing a flow sequence at line 3 column 6"
    );
}

#[test]
fn decode_errors_are_pinned() {
    // Decoded straight from the document: path and position included.
    assert_eq!(
        read_error("version: '3'\nvars:\n  V: 18446744073709551616\n"),
        "task: Failed to parse Taskfile.yml:\ntask: Failed to decode Taskfile: vars.V: invalid type: integer `18446744073709551616` as u128, expected any YAML value at line 3 column 6"
    );
    // Decoded after a `from_value` hop: path and position are lost.
    assert_eq!(
        read_error("version: '3'\ntasks:\n  build:\n    vars:\n      X:\n        sh: [1]\n"),
        "task: Failed to parse Taskfile.yml:\ntask: Failed to decode Taskfile: task: Failed to decode Taskfile: task: Failed to decode Taskfile: task: Failed to decode Taskfile: task: Failed to decode Taskfile: invalid type: sequence, expected a string"
    );
    assert_eq!(
        read_error("version: '3'\nvars:\n  X:\n    foo: 1\n"),
        "task: Failed to parse Taskfile.yml:\ntask: Failed to decode Taskfile: task: Failed to decode Taskfile: task: Failed to decode Taskfile: \"foo\" is not a valid variable type. Try \"sh\", \"ref\", \"map\" or using a scalar value"
    );
    assert_eq!(
        read_error("version: '3'\n---\nversion: '3'\n"),
        "task: Failed to parse Taskfile.yml:\ntask: Failed to decode Taskfile: deserializing from YAML containing more than one document is not supported"
    );
}
