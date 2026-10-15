//! Parity with serde_yaml_ng: expectations are what serde_yaml_ng 0.10
//! produced for the same input, except syntax errors (worded and sometimes
//! positioned differently) and the cases in `deliberate_divergences_from_go`.

use serde::Deserialize;

use super::{Mapping, Tag, TaggedValue, Value, from_slice, from_str, from_value, to_string};

fn parse(yaml: &str) -> Value {
    from_str(yaml).unwrap()
}

/// The value under `key` in a mapping.
fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    match value {
        Value::Mapping(map) => map.get(key).unwrap(),
        other => panic!("not a mapping: {other:?}"),
    }
}

fn error(yaml: &str) -> String {
    from_str::<Value>(yaml).unwrap_err().to_string()
}

/// Plain scalars are typed by serde_yaml_ng's rules, not YAML 1.2's: leading
/// zeros and `_` keep a string, only three spellings make a boolean or null.
const SCALARS: &[(&str, &str)] = &[
    ("017", r#"String("017")"#),
    ("0022", r#"String("0022")"#),
    ("08", r#"String("08")"#),
    ("00", r#"String("00")"#),
    ("1_000", r#"String("1_000")"#),
    ("0x1_F", r#"String("0x1_F")"#),
    ("0O17", r#"String("0O17")"#),
    ("0B1", r#"String("0B1")"#),
    ("tRuE", r#"String("tRuE")"#),
    ("nUlL", r#"String("nUlL")"#),
    (".Nan", r#"String(".Nan")"#),
    ("1e400", r#"String("1e400")"#),
    (".inf", "Number(.inf)"),
    ("-.Inf", "Number(-.inf)"),
    (".NAN", "Number(.nan)"),
    ("0x1F", "Number(31)"),
    ("0o17", "Number(15)"),
    ("0b11", "Number(3)"),
    ("-0x1F", "Number(-31)"),
    ("+12", "Number(12)"),
    ("True", "Bool(true)"),
    ("FALSE", "Bool(false)"),
    ("yes", r#"String("yes")"#),
    ("off", r#"String("off")"#),
    ("~", "Null"),
    ("Null", "Null"),
    ("", "Null"),
    ("1.0", "Number(1.0)"),
    ("1e3", "Number(1000.0)"),
    ("6.02e23", "Number(6.02e23)"),
    ("1e20", "Number(1e20)"),
    ("0.1", "Number(0.1)"),
    ("-0.0", "Number(-0.0)"),
    ("00.5", "Number(0.5)"),
    ("18446744073709551615", "Number(18446744073709551615)"),
    ("-9223372036854775808", "Number(-9223372036854775808)"),
    ("!!str 123", r#"String("123")"#),
    ("!!int '7'", "Number(7)"),
    ("!!binary aGk=", r#"String("aGk=")"#),
    (
        "!foo bar",
        r#"TaggedValue { tag: !foo, value: String("bar") }"#,
    ),
    ("!<tag:example.com,2000:x> 5", r#"String("5")"#),
    ("'0x1F'", r#"String("0x1F")"#),
    ("\"1\"", r#"String("1")"#),
];

#[test]
fn scalars_are_typed_as_serde_yaml_ng_did() {
    for (text, want) in SCALARS {
        assert_eq!(format!("{:?}", parse(text)), *want, "{text}");
        // The same as a mapping value, through an alias, and in a sequence.
        let doc = parse(&format!("a: &x {text}\nb: *x\nc: [{text}]\n"));
        for key in ["a", "b"] {
            assert_eq!(format!("{:?}", get(&doc, key)), *want, "{text}");
        }
        // `[]` is an empty sequence.
        let Value::Sequence(seq) = get(&doc, "c") else {
            panic!("c is not a sequence");
        };
        if !text.is_empty() {
            assert_eq!(format!("{:?}", seq.first().unwrap()), *want, "{text}");
        }
    }
}

#[test]
fn keys_are_typed_like_values() {
    for (text, want) in SCALARS {
        // An empty or null key is not a valid flow key on its own line.
        if text.is_empty() || text.starts_with('!') {
            continue;
        }
        let doc = parse(&format!("{text}: v\n"));
        let Value::Mapping(map) = &doc else {
            panic!("{text}: not a mapping");
        };
        let (key, _) = map.iter().next().unwrap();
        assert_eq!(format!("{key:?}"), *want, "{text}");
    }
}

#[test]
fn integers_past_64_bits_are_errors() {
    assert_eq!(
        error("a: 18446744073709551616\n"),
        "a: invalid type: integer `18446744073709551616` as u128, expected any YAML value at line 1 column 4"
    );
    assert_eq!(
        error("a: -9223372036854775809\n"),
        "a: invalid type: integer `-9223372036854775809` as i128, expected any YAML value at line 1 column 4"
    );
    // The node starts at its anchor.
    assert_eq!(
        error("k: &x 99999999999999999999\n"),
        "k: invalid type: integer `99999999999999999999` as u128, expected any YAML value at line 1 column 4"
    );
}

#[test]
fn merge_keys_stay_literal() {
    let doc = parse("base: &b {x: 1}\nc:\n  <<: *b\n  y: 2\n");
    let c = get(&doc, "c");
    assert_eq!(
        format!("{c:?}"),
        r#"Mapping {"<<": Mapping {"x": Number(1)}, "y": Number(2)}"#
    );
}

#[test]
fn duplicate_keys_are_errors() {
    assert_eq!(
        error("a: 1\nb:\n  x: 1\n  x: 2\n"),
        r#"b: duplicate entry with key "x" at line 3 column 3"#
    );
    // No position at the very start of the input.
    assert_eq!(error("{1: a, 1: b}"), "duplicate entry with key 1");
    assert_eq!(error("~: a\nnull: b\n"), "duplicate entry with null key");
}

#[test]
fn errors_name_the_path_and_position() {
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct Task {
        cmds: Vec<String>,
    }
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct File {
        tasks: std::collections::BTreeMap<String, Task>,
    }
    let err = from_str::<File>("tasks:\n  a:\n    cmds:\n      - x\n      - {y: 1}\n").unwrap_err();
    assert_eq!(
        err.to_string(),
        "tasks.a.cmds[1]: invalid type: map, expected a string at line 5 column 9"
    );
    let location = err.location().unwrap();
    assert_eq!((location.line(), location.column()), (5, 9));

    // Empty values and block scalars are marked where libyaml marked them.
    let err = from_str::<File>("tasks:\n  a:\n  b:\n    cmds: []\n").unwrap_err();
    assert_eq!(
        err.to_string(),
        "tasks.a: missing field `cmds` at line 2 column 5"
    );
    let err = from_str::<File>("tasks:\n  a:\n    cmds: |\n      x\n").unwrap_err();
    assert_eq!(
        err.to_string(),
        "tasks.a.cmds: invalid type: string \"x\\n\", expected a sequence at line 3 column 11"
    );
}

#[test]
fn document_level_errors() {
    assert_eq!(
        error("a: 1\n---\nb: 2\n"),
        "deserializing from YAML containing more than one document is not supported"
    );
    assert_eq!(error("a: *nope\n"), "unknown anchor at line 1 column 4");
    let err = from_slice::<Value>(b"a: \xff\n").unwrap_err();
    assert_eq!(err.to_string(), "invalid leading UTF-8 octet at position 3");
    assert_eq!(err.location().map(|l| (l.line(), l.column())), Some((1, 1)));
    assert_eq!(
        error("a: \"\x1b\"\n"),
        "control characters are not allowed at position 4"
    );
}

#[test]
fn underindented_quoted_continuations_are_accepted() {
    // libyaml accepted a quoted scalar continuing at its key's indentation;
    // YAML 1.2 does not.
    let doc = parse("tasks:\n  x:\n    desc: 'a\n    b'\n    summary: \"c\nd\"\n");
    let x = get(get(&doc, "tasks"), "x");
    assert_eq!(get(x, "desc").as_str(), Some("a b"));
    assert_eq!(get(x, "summary").as_str(), Some("c d"));
}

#[test]
fn tab_led_quoted_continuations_are_accepted() {
    // libyaml and go-yaml read these; the parser refuses a tab there.
    for (yaml, want) in [
        ("a: \"x\n\ty\"\n", r#"Mapping {"a": String("x y")}"#),
        ("- \"echo a\n\tb\"\n", r#"Sequence [String("echo a b")]"#),
        ("b: 'x\n\n\ty'\n", r#"Mapping {"b": String("x\ny")}"#),
        (
            "x:\n  - \"echo a\n\tb\"\n",
            r#"Mapping {"x": Sequence [String("echo a b")]}"#,
        ),
        (
            "f: [\"y\n\tz\"]",
            r#"Mapping {"f": Sequence [String("y z")]}"#,
        ),
        (
            "f: [ \"y\n\tz\"]",
            r#"Mapping {"f": Sequence [String("y z")]}"#,
        ),
        (
            "f:\n  - [\"y\n\tz\"]",
            r#"Mapping {"f": Sequence [Sequence [String("y z")]]}"#,
        ),
    ] {
        assert_eq!(format!("{:?}", parse(yaml)), want, "{yaml:?}");
    }
    // A tab outside a quoted scalar is still refused.
    assert_eq!(
        error("a:\n\tb: 1\n"),
        "tabs disallowed within this context (block indentation) at line 2 column 2"
    );
}

#[test]
fn quotes_in_block_scalars_are_left_alone() {
    let doc = parse("k:\n  desc: \"a\nb\"\n  c: |\n    echo \"q\n    r\"\n  d: 'e\nf'\n");
    assert_eq!(
        format!("{doc:?}"),
        r#"Mapping {"k": Mapping {"desc": String("a b"), "c": String("echo \"q\nr\"\n"), "d": String("e f")}}"#
    );
}

#[test]
fn false_quotes_are_undone_when_a_later_scalar_stops_the_parse() {
    assert_eq!(
        format!("{:?}", parse("a: 'x\ny'\nb: |\n  'foo\nc: 'p\nq'\n")),
        r#"Mapping {"a": String("x y"), "b": String("'foo\n"), "c": String("p q")}"#
    );
    assert_eq!(
        format!("{:?}", parse("a: 'x\ny'\nb: |\n  \"foo\nc: 'p\nq'\n")),
        r#"Mapping {"a": String("x y"), "b": String("\"foo\n"), "c": String("p q")}"#
    );
}

#[test]
fn errors_after_a_reindent_are_reported_where_they_are() {
    assert_eq!(
        error("b: 'x\ny'\nc: \"x\"#c\n"),
        "comments must be separated from other tokens by whitespace at line 3 column 7"
    );
    assert_eq!(
        error("b: 'x\ny'\nb: - 'foo\n"),
        r#"duplicate entry with key "b""#
    );
}

#[test]
fn tab_led_lines_in_flow_collections_are_accepted() {
    // libyaml and go-yaml read these; the parser refuses a leading tab.
    assert_eq!(
        format!(
            "{:?}",
            parse("tasks:\n  a:\n    cmds: [\n\techo a,\n\techo b\n    ]\n")
        ),
        r#"Mapping {"tasks": Mapping {"a": Mapping {"cmds": Sequence [String("echo a"), String("echo b")]}}}"#
    );
    assert_eq!(
        format!("{:?}", parse("a: {\n\tk: v}")),
        r#"Mapping {"a": Mapping {"k": String("v")}}"#
    );
    // Tab indentation in block context stays an error.
    assert!(from_str::<Value>("a:\n\tb: 1\n").is_err());
}

#[test]
fn a_block_scalar_ending_the_input_keeps_no_phantom_line_break() {
    // As libyaml and go-yaml: a last line without a line break has none to keep.
    for (yaml, want) in [
        ("a: |\n  echo hi", "echo hi"),
        ("a: |\n  echo hi\n", "echo hi\n"),
        ("a: >\n  x\n  y", "x y"),
        ("a: |+\n  x", "x"),
        ("a: |\n  x\n  ", "x\n"),
        ("a: |\n  x\n  y  ", "x\ny  "),
        ("a: |\n  x\n# c", "x\n"),
    ] {
        assert_eq!(get(&parse(yaml), "a").as_str(), Some(want), "{yaml:?}");
    }
}

#[test]
fn a_long_line_of_quotes_scans_in_linear_time() {
    // Half a megabyte on one line. Re-indenting must cost about one more
    // parse, not grow with the number of quotes on the line.
    let doc = |head: &str| format!("{head}b: [{}]\n", "'', ".repeat(125_000));
    let time = |yaml: &str| {
        let start = std::time::Instant::now();
        let doc = parse(yaml);
        assert!(matches!(get(&doc, "b"), Value::Sequence(seq) if seq.len() == 125_000));
        start.elapsed()
    };
    let plain = time(&doc("a: 'x y'\n"));
    let reindented = time(&doc("a: 'x\ny'\n"));
    assert!(reindented < plain * 4, "{reindented:?} vs {plain:?}");
}

#[test]
fn document_markers_end_quoted_scalars() {
    // A column-0 `---` or `...` is a document marker, as for libyaml and go-yaml,
    // reported where it is.
    for (yaml, line) in [
        ("a: \"x\n---\ny\"\n", 2),
        ("a: \"x\n...\ny\"\n", 2),
        ("a: 'x\n--- \ny'\n", 2),
        ("a: 'x\n...\nb: 'y'\n", 2),
        ("k:\n  a: \"x\n---\ny\"\n", 3),
        // After a re-indented scalar.
        ("a: 'p\nq'\nb: \"x\n---\ny\"\n", 4),
    ] {
        assert_eq!(
            error(yaml),
            format!("unexpected document indicator in quoted scalar at line {line} column 1"),
            "{yaml:?}"
        );
    }
    assert_eq!(
        get(&parse("a: 'x\n---x\ny'\n"), "a").as_str(),
        Some("x ---x y")
    );
}

#[test]
fn unclosed_quotes_report_the_original_position() {
    let mut yaml = String::from("version: '3'\ntasks:\n  a:\n    desc: \"oops\n");
    for i in 0..2000 {
        yaml.push_str(&format!("  t{i}:\n    cmds:\n      - echo {i}\n"));
    }
    assert_eq!(error(&yaml), "unclosed quote at line 4 column 11");
}

#[test]
fn unclosed_quotes_are_reported_where_they_open() {
    // The parser reads on past the line and refuses a later line's
    // indentation; libyaml reported the end of input.
    for (yaml, want) in [
        (
            "version: '3\nfoo: bar\n",
            "unclosed quote at line 1 column 10",
        ),
        (
            "a:\n  b: \"x\n  c: 1\n",
            "unclosed quote at line 2 column 6",
        ),
        ("a: \"x\n\tb: 1\n", "unclosed quote at line 1 column 4"),
        // After a re-indented scalar, in the original text.
        (
            "a: \"ok\n  x\"\nb: \"y\n  z\n",
            "unclosed quote at line 3 column 4",
        ),
        // At the end of input, the parser says so itself.
        ("a: 'x", "unclosed quote at line 1 column 4"),
    ] {
        assert_eq!(error(yaml), want, "{yaml:?}");
    }
    // A quote that closes is not blamed.
    assert_eq!(
        error("a: 'x\n  b: c\nd: 'e\n"),
        "invalid indentation in multiline quoted scalar at line 3 column 1"
    );
}

#[test]
fn many_underindented_lines_load_quickly() {
    let mut yaml = String::from("tasks:\n  x:\n    desc: \"start\n");
    for i in 0..5000 {
        yaml.push_str(&format!("line {i}\n"));
    }
    yaml.push_str("end\"\n    cmds: [echo]\n");
    for i in 0..2000 {
        yaml.push_str(&format!("  t{i}:\n    desc: 'a\n    b'\n"));
    }
    let start = std::time::Instant::now();
    let doc = parse(&yaml);
    let desc = get(get(get(&doc, "tasks"), "x"), "desc").as_str().unwrap();
    assert!(desc.starts_with("start line 0 line 1 ") && desc.ends_with(" line 4999 end"));
    assert_eq!(
        get(get(get(&doc, "tasks"), "t1999"), "desc").as_str(),
        Some("a b")
    );
    // Generous for a debug build; it takes a few parses, not one per line.
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn positions_after_a_reindent_are_in_the_original_text() {
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct File {
        a: String,
        b: u8,
    }
    let err = from_str::<File>("a: 'x\ny'\nb: z\n").unwrap_err();
    assert_eq!(
        err.to_string(),
        r#"b: invalid type: string "z", expected u8 at line 3 column 4"#
    );
}

#[test]
fn block_scalars_ending_before_a_long_last_line_load() {
    // Only a block scalar at the end of input looks for the last line, so
    // loading stays linear rather than quadratic in the number of scalars.
    let mut yaml = String::new();
    for i in 0..2000 {
        yaml.push_str(&format!("k{i}: |\n  v\n"));
    }
    yaml.push_str(&format!("last: |\n  {}", "x".repeat(200_000)));
    let doc = parse(&yaml);
    assert_eq!(get(&doc, "k1999").as_str(), Some("v\n"));
    let last = get(&doc, "last").as_str().unwrap();
    assert_eq!(last.len(), 200_000);
    assert!(!last.ends_with('\n'));
}

#[test]
fn nesting_is_limited() {
    let nested = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    assert!(from_str::<Value>(&nested(128)).is_ok());
    // Deserializing stops at the 129th level.
    assert_eq!(
        error(&nested(129)),
        "recursion limit exceeded at line 1 column 129"
    );
    assert_eq!(
        error(&nested(255)),
        "recursion limit exceeded at line 1 column 129"
    );
    // Past 255 levels the parser stops first, at its own limit.
    assert_eq!(
        error(&nested(256)),
        "recursion limit exceeded at line 1 column 256"
    );
    assert_eq!(
        error(&nested(1000)),
        "recursion limit exceeded at line 1 column 256"
    );
}

#[test]
fn alias_expansion_is_limited() {
    // A billion laughs: nine levels of nine aliases each.
    let mut yaml = String::from("a: &a [x, x]\n");
    let mut prev = 'a';
    for name in ['b', 'c', 'd', 'e', 'f', 'g', 'h', 'i'] {
        let refs = vec![format!("*{prev}"); 9].join(", ");
        yaml.push_str(&format!("{name}: &{name} [{refs}]\n"));
        prev = name;
    }
    assert_eq!(error(&yaml), "repetition limit exceeded");
    // An alias inside the node it refers to recurses until the depth limit.
    assert_eq!(error("&a [*a]\n"), "recursion limit exceeded");
    assert_eq!(
        error("a: &a\n  b: *a\n"),
        "recursion limit exceeded at line 1 column 4"
    );
}

#[test]
fn a_second_document_is_refused_even_when_malformed() {
    assert_eq!(
        error("a: 1\n---\nb: [\n"),
        "deserializing from YAML containing more than one document is not supported"
    );
}

/// Inputs where the parser and go-yaml (Go Task) disagree.
#[test]
fn deliberate_divergences_from_go() {
    // Go reads these as the values before the comment or `-`; refused here.
    for yaml in ["a: 'x'#c\n", "a: \"x\"#c\n", "a: |#c\n  x\n", "[-]"] {
        assert!(from_str::<Value>(yaml).is_err(), "{yaml:?}");
    }
    // Go reads `[?x]` as `[{x: null}]`.
    assert_eq!(format!("{:?}", parse("[?x]")), r#"Sequence [String("?x")]"#);
    // Go ends an anchor name at `:`, reading `":y 1"` here.
    assert_eq!(
        format!("{:?}", parse("a: &x:y 1\n")),
        r#"Mapping {"a": Number(1)}"#
    );
    // Go refuses a line separator or next line in a plain scalar, as line
    // breaks; here they are part of the value.
    assert_eq!(
        get(&parse("a: x\u{2028}y\n"), "a").as_str(),
        Some("x\u{2028}y")
    );
    assert_eq!(get(&parse("a: x\u{85}y\n"), "a").as_str(), Some("x\u{85}y"));
    assert_eq!(
        get(&parse("a: x\u{2029}y\n"), "a").as_str(),
        Some("x\u{2029}y")
    );
    // The emitter still writes them as libyaml did, as line breaks, so such
    // a string does not read back the same.
    let emitted = to_string(&parse("a: x\u{2028}y\n")).unwrap();
    assert_eq!(emitted, "a: 'x\u{2028}  y'\n");
    assert_ne!(get(&parse(&emitted), "a").as_str(), Some("x\u{2028}y"));
    // Go refuses a stream of only `...`, a `%YAML 1.3` directive and `#`
    // in an anchor name; Go reads a comment glued to a flow collection or
    // after a flow `,`, refused here.
    assert_eq!(parse("...\n"), Value::Null);
    assert_eq!(
        format!("{:?}", parse("%YAML 1.3\n---\na: 1\n")),
        r#"Mapping {"a": Number(1)}"#
    );
    assert_eq!(
        format!("{:?}", parse("a: &a#c x\nb: *a#c\n")),
        r#"Mapping {"a": String("x"), "b": String("x")}"#
    );
    for yaml in ["[1,#c\n2]\n", "a: {b: 1}#c\n"] {
        assert!(from_str::<Value>(yaml).is_err(), "{yaml:?}");
    }
    // Go refuses tab indentation, a tab-only line, an empty key and `[?]`.
    for (yaml, want) in [
        ("\ta: 1\n", r#"Mapping {"a": Number(1)}"#),
        (
            "a: 1\n\t\nb: 2\n",
            r#"Mapping {"a": Number(1), "b": Number(2)}"#,
        ),
        (": x\n", r#"Mapping {Null: String("x")}"#),
        ("[?]", r#"Sequence [String("?")]"#),
        ("- a\n-\tb\n", r#"Sequence [String("a"), String("b")]"#),
    ] {
        assert_eq!(format!("{:?}", parse(yaml)), want, "{yaml:?}");
    }
    // A leading byte order mark loads, as in Go; libyaml read a multi-line
    // file starting with one as two documents.
    assert_eq!(
        format!("{:?}", parse("\u{feff}a: 1\nb: 2\n")),
        r#"Mapping {"a": Number(1), "b": Number(2)}"#
    );
}

#[test]
fn numbers_display_as_serde_yaml_ng_did() {
    for (text, want) in [
        ("1", "1"),
        ("-1", "-1"),
        ("1.0", "1.0"),
        ("1e3", "1000.0"),
        ("1e20", "1e20"),
        ("6.02e23", "6.02e23"),
        ("0.1", "0.1"),
        ("1e-7", "1e-7"),
        (".inf", ".inf"),
        ("-.inf", "-.inf"),
        (".nan", ".nan"),
    ] {
        let Value::Number(n) = parse(text) else {
            panic!("{text} is not a number");
        };
        assert_eq!(n.to_string(), want, "{text}");
    }
}

#[test]
fn from_value_coerces_as_serde_yaml_ng_did() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct S {
        n: u8,
        s: Option<String>,
        v: Vec<String>,
    }
    let s: S = from_value(parse("n: 7\ns: ~\nv: ~\n")).unwrap();
    assert_eq!(
        s,
        S {
            n: 7,
            s: None,
            v: Vec::new()
        }
    );
    let err = from_value::<S>(parse("n: x\ns: ~\nv: []\n")).unwrap_err();
    assert_eq!(err.to_string(), r#"invalid type: string "x", expected u8"#);
    // A tag is seen through when a plain type is expected.
    let s: String = from_value(parse("!t x")).unwrap();
    assert_eq!(s, "x");
}

#[test]
fn values_serialize_as_serde_yaml_ng_did() {
    let tagged = Value::Tagged(Box::new(TaggedValue {
        tag: Tag::new("t"),
        value: Value::from(5),
    }));
    assert_eq!(serde_json::to_string(&tagged).unwrap(), r#"{"!t":5}"#);
    assert_eq!(
        serde_json::to_string(&parse("[1, -2, 1.5, x, ~, true]")).unwrap(),
        r#"[1,-2,1.5,"x",null,true]"#
    );
}

#[test]
fn to_string_emits_as_serde_yaml_ng_did() {
    let s = |text: &str| Value::String(text.to_string());
    let mut inner = Mapping::new();
    inner.insert(s("x"), Value::from(1));
    let tagged = |tag: &str, value: Value| {
        Value::Tagged(Box::new(TaggedValue {
            tag: Tag::new(tag),
            value,
        }))
    };
    let mut m = Mapping::new();
    m.insert(
        s("a"),
        Value::Sequence(vec![Value::from(1), Value::from(2)]),
    );
    m.insert(s("b"), Value::Mapping(inner.clone()));
    m.insert(s("e"), tagged("foo", Value::Mapping(inner.clone())));
    m.insert(s("g"), Value::Sequence(Vec::new()));
    m.insert(s("j"), s("multi\nline"));
    m.insert(s("l"), s("keep\n\n"));
    m.insert(s("m"), s(" lead"));
    m.insert(s("n"), s("tab\there"));
    m.insert(s("o"), s("a: b"));
    m.insert(s("s"), s("it's"));
    m.insert(s("num"), s("1.5"));
    m.insert(s("hex"), s("0x1F"));
    m.insert(s("zeros"), s("0022"));
    m.insert(s("yes"), s("yes"));
    m.insert(s(""), s(""));
    m.insert(
        s("u"),
        Value::Sequence(vec![
            Value::Mapping(inner.clone()),
            Value::Sequence(vec![Value::from(1)]),
            tagged("q", s("v")),
        ]),
    );
    m.insert(Value::Sequence(vec![Value::from(1)]), s("seqkey"));
    m.insert(tagged("k", s("x")), s("tagkey"));
    m.insert(Value::Null, Value::Bool(false));
    let want = "\
a:
- 1
- 2
b:
  x: 1
e: !foo
  x: 1
g: []
j: |-
  multi
  line
l: |+
  keep

m: ' lead'
n: \"tab\\there\"
o: 'a: b'
s: it's
num: '1.5'
hex: '0x1F'
zeros: '0022'
yes: yes
'': ''
u:
- x: 1
- - 1
- !q v
? - 1
: seqkey
!k x: tagkey
null: false
";
    assert_eq!(to_string(&Value::Mapping(m)).unwrap(), want);
    assert_eq!(to_string(&Value::from(1)).unwrap(), "1\n");
    assert_eq!(to_string(&s("a\n\n")).unwrap(), "|+\n  a\n\n");
    assert_eq!(
        to_string(&tagged("t", tagged("u", Value::Null)))
            .unwrap_err()
            .to_string(),
        "serializing nested enums in YAML is not supported yet"
    );
}
