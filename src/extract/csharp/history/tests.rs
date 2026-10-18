use super::*;
use std::collections::BTreeSet;

fn project(source: &str, conditions: &mut Conditions, values: &[bool]) -> Result<Projection> {
    let context = Context::whole(source.len());
    Document::new(source, conditions)?.project(conditions, values, &context.ranges)
}

fn signatures(facts: &HistoricalFacts) -> BTreeSet<(String, Vec<String>)> {
    facts
        .declarations
        .iter()
        .filter(|d| d.name == "Target")
        .map(|d| (d.ty.clone(), d.parameters.clone()))
        .collect()
}

#[test]
fn raw_verbatim_and_nested_interpolation_text_is_not_preprocessing() {
    for source in [
        r#"class Worker {
    string Banner = """"
        #if FEATURE
        """
        #else
        """";
    int Target() { return 1; }
}"#,
        r#"class Worker {
    string Banner = @$"{F("nested")}
        #if FEATURE
        #endif
        ";
    int Target() { return 1; }
}"#,
        r#"class Worker {
    string Banner = $$"""
        #if FEATURE
        {{F("nested", new[] { "}" })}}
        #else
        """;
    int Target() { return 1; }
}"#,
        r#"/*
#if NOT_A_DIRECTIVE
*/
class Worker {
    string Banner = $"{F(@"
#if STILL_TEXT
")}";
    int Target() { return 1; }
}"#,
    ] {
        let projected = project(source, &mut Conditions::default(), &[false; 128]).unwrap();
        assert_eq!(projected.text, source);
        assert_eq!(projected.guard, TRUE);
        let facts = extract(source).unwrap();
        assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
        assert_eq!(signatures(&facts), BTreeSet::from([("int".into(), vec![])]));
    }
}

#[test]
fn repeated_conditions_correlate_return_and_parameter_alternatives() {
    let source = "class Worker {\n#if FEATURE\nint\n#else\nstring\n#endif\nTarget(\n#if FEATURE\nint value\n#else\nstring value\n#endif\n) { return default; }\n}";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(
        signatures(&facts),
        BTreeSet::from([
            ("int".into(), vec!["int".into()]),
            ("string".into(), vec!["string".into()]),
        ])
    );
}

#[test]
fn independent_optional_parameters_keep_every_feasible_written_signature() {
    let source = "class Worker { void Target(int seed\n#if A\n, int first\n#endif\n#if B\n, string second\n#endif\n) {} }";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(
        signatures(&facts),
        BTreeSet::from([
            ("void".into(), vec!["int".into()]),
            ("void".into(), vec!["int".into(), "int".into()]),
            ("void".into(), vec!["int".into(), "string".into()]),
            (
                "void".into(),
                vec!["int".into(), "int".into(), "string".into()]
            ),
        ])
    );
    let methods: Vec<_> = facts
        .declarations
        .iter()
        .filter(|d| d.name == "Target")
        .collect();
    assert!(
        methods
            .iter()
            .all(|d| d.span == methods[0].span && !d.uncertain)
    );
    assert_eq!(
        methods
            .iter()
            .map(|d| &d.written)
            .collect::<BTreeSet<_>>()
            .len(),
        4
    );
}

#[test]
fn constants_elif_precedence_and_written_symbol_changes_prune_impossible_branches() {
    let source = "#define ON\n#undef OFF\n#if A\n#define CHOSEN\n#else\n#undef CHOSEN\n#endif\nclass Worker {\n#if ON && !OFF && (A == CHOSEN)\nint Target(int value) { return value; }\n#elif !ON || OFF || (A != CHOSEN)\nint Target(string impossible) { return 0; }\n#endif\n#if A\nint First;\n#elif A\nint Never;\n#elif(!A)// no intervening space needed\nint Second;\n#else// unreachable\nint AlsoNever;\n#endif\n}";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(
        signatures(&facts),
        BTreeSet::from([("int".into(), vec!["int".into()])])
    );
    assert!(facts.declarations.iter().any(|d| d.name == "First"));
    assert!(facts.declarations.iter().any(|d| d.name == "Second"));
    assert!(!facts.declarations.iter().any(|d| d.name.contains("Never")));
}

#[test]
fn inactive_text_does_not_poison_the_lexical_state() {
    let source = "class Worker {\n#if false\n/* \"\"\" unterminated disabled text\n#if UNUSED\nnot C#\n#endif\n#else\nint Target(int value) { return value; }\n#endif\n}";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(
        signatures(&facts),
        BTreeSet::from([("int".into(), vec!["int".into()])])
    );
}

#[test]
fn projection_preserves_utf8_offsets_bom_and_every_line_ending() {
    let source = "\u{feff}#region Sample\r\nclass Café {\r\n    int Target(\r\n# if(FEATURE)\r\nint π\r\n#else// alternate\r\nstring π\r\n#endif\r\n) { return 1; }\r\n}\r\n#endregion";
    for yes in [false, true] {
        let projected = project(source, &mut Conditions::default(), &[yes; 128]).unwrap();
        assert_eq!(source.len(), projected.text.len());
        assert_eq!(source.find("Café"), projected.text.find("Café"));
        assert_eq!(source.find("Target"), projected.text.find("Target"));
        let breaks = |s: &str| {
            s.char_indices()
                .filter(|(_, c)| newline(*c))
                .collect::<Vec<_>>()
        };
        assert_eq!(breaks(source), breaks(&projected.text));
        assert!(
            projected
                .text
                .contains(if yes { "int π" } else { "string π" })
        );
        assert!(
            !projected
                .text
                .contains(if yes { "string π" } else { "int π" })
        );
    }
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    for declaration in facts.declarations.iter().filter(|d| d.name == "Target") {
        assert_eq!(&source[declaration.name_span.clone()], "Target");
        assert!(source[declaration.span.clone()].contains("# if(FEATURE)\r\n"));
    }
}

#[test]
fn malformed_directives_and_unfinished_headers_remain_explicit() {
    for directive in [
        "#endif",
        "#else",
        "#elif A",
        "#if A",
        "#if A & B\n#endif",
        "#if A\n#else\n#else\n#endif",
        "#if A\n#else\n#elif B\n#endif",
        "#if_FEATURE",
        "#if1",
        "#define true",
        "#region Unclosed",
    ] {
        let source = format!("{directive}\nclass Worker {{ int Target() {{ return 1; }} }}");
        let facts = extract(&source).unwrap();
        assert!(facts.incomplete.is_some(), "{directive}");
        assert!(
            facts.declarations.iter().all(|d| d.uncertain),
            "{directive}"
        );
    }
    let facts = extract("class Worker { int Target(int value, ) { return 1; }").unwrap();
    assert!(facts.incomplete.is_some());
}

#[test]
fn unsafe_expression_trees_keep_the_written_operands() {
    let source = r#"unsafe class Worker {
    static void Target(byte* pointer, ref int value) {
        *pointer++ = 1;
        Unsafe.AsRef(in value) = 2;
        (Unsafe.AsRef(in value)) = 3;
        Sink(ref *(byte*)pointer);
        var sum = *pointer + 1;
        *(pointer + 1) = 4;
    }
}"#;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error());
    let node = |text: &str| {
        let start = source.find(text).unwrap();
        let range = start..start + text.len();
        let node = tree
            .root_node()
            .descendant_for_byte_range(range.start, range.end)
            .unwrap();
        assert_eq!(node.byte_range(), range);
        node
    };

    let pointer = node("*pointer++");
    assert_eq!(pointer.kind(), "prefix_unary_expression");
    assert_eq!(
        pointer.named_child(0).unwrap().kind(),
        "postfix_unary_expression"
    );
    assert_eq!(pointer.named_child(0).unwrap(), node("pointer++"));

    let assignment = node("Unsafe.AsRef(in value) = 2");
    assert_eq!(assignment.kind(), "assignment_expression");
    assert_eq!(
        assignment.child_by_field_name("left").unwrap(),
        node("Unsafe.AsRef(in value)")
    );
    assert_eq!(
        assignment.child_by_field_name("left").unwrap().kind(),
        "invocation_expression"
    );
    let parenthesized = node("(Unsafe.AsRef(in value)) = 3");
    assert_eq!(
        parenthesized.child_by_field_name("left").unwrap().kind(),
        "parenthesized_expression"
    );

    let argument = node("ref *(byte*)pointer");
    assert_eq!(argument.kind(), "argument");
    let dereference = argument.named_child(0).unwrap();
    assert_eq!(dereference, node("*(byte*)pointer"));
    assert_eq!(dereference.kind(), "prefix_unary_expression");
    let cast = dereference.named_child(0).unwrap();
    assert_eq!(cast.kind(), "cast_expression");
    let cast_type = cast.child_by_field_name("type").unwrap();
    assert_eq!(cast_type.kind(), "pointer_type");
    assert_eq!(&source[cast_type.byte_range()], "byte*");

    let addition = node("*pointer + 1");
    assert_eq!(addition.kind(), "binary_expression");
    assert_eq!(
        addition.child_by_field_name("left").unwrap().kind(),
        "prefix_unary_expression"
    );
    let arithmetic = node("*(pointer + 1)");
    assert_eq!(arithmetic.kind(), "prefix_unary_expression");
    assert_eq!(
        arithmetic.named_child(0).unwrap().kind(),
        "parenthesized_expression"
    );
}

#[test]
fn unfinished_unsafe_expressions_still_report_parse_errors() {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .unwrap();
    for body in ["return +;", "*pointer++ = ;", "Sink(ref *(byte*) );"] {
        let source = format!("unsafe class Worker {{ void Target(byte* pointer) {{ {body} }} }}");
        assert!(parser.parse(&source, None).unwrap().root_node().has_error());
        let facts = extract(&source).unwrap();
        assert!(facts.incomplete.is_some());
        assert!(
            facts
                .declarations
                .iter()
                .all(|declaration| declaration.uncertain)
        );
    }
}

#[test]
fn legal_unsafe_expressions_keep_historical_member_boundaries() {
    let source = r#"unsafe class Worker {
    static void Target(byte* pointer, ref int value) {
        *pointer++ = 1;
        Unsafe.AsRef(in value) = 2;
        Sink(ref *(byte*)pointer);
    }
}"#;
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert!(
        facts
            .declarations
            .iter()
            .any(|d| d.name == "Target" && !d.uncertain)
    );
}

#[test]
fn recovered_syntax_never_supplies_a_strong_identity() {
    let source =
        "class Worker { int Broken() { return +; } int Target(int value) { return value; } }";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_some());
    assert!(facts.declarations.iter().any(|d| d.name == "Target"));
    assert!(facts.declarations.iter().all(|d| d.uncertain));
}

#[test]
fn repeated_groups_do_not_enumerate_independent_decisions_for_one_symbol() {
    let mut source = "class Worker {\n".to_owned();
    for index in 0..256 {
        source.push_str(&format!(
            "void M{index}(\n#if A\nint value\n#else\nstring value\n#endif\n) {{}}\n"
        ));
    }
    source.push('}');
    let facts = extract(&source).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(
        facts
            .declarations
            .iter()
            .filter(|d| d.kind == "method")
            .count(),
        512
    );
    assert!(facts.declarations.iter().all(|d| !d.uncertain));
}

#[test]
fn independent_explosion_and_deep_nesting_stop_with_explicit_uncertainty() {
    let mut source = "class Worker { void Target(int seed\n".to_owned();
    for index in 0..16 {
        source.push_str(&format!("#if A{index}\n, int p{index}\n#endif\n"));
    }
    source.push_str(") {} }");
    let facts = extract(&source).unwrap();
    assert!(
        facts
            .incomplete
            .as_deref()
            .is_some_and(|s| s.contains("limit reached")),
        "{:?}",
        facts.incomplete
    );
    assert!(
        facts
            .declarations
            .iter()
            .filter(|d| d.name == "Target")
            .count()
            <= PROJECTIONS
    );
    assert!(facts.declarations.iter().all(|d| d.uncertain));
    assert!(facts.occurrences.is_empty());

    let nested = format!(
        "{}class Worker {{}}\n{}",
        "#if A\n".repeat(10_000),
        "#endif\n".repeat(10_000)
    );
    let facts = extract(&nested).unwrap();
    assert!(
        facts
            .incomplete
            .as_deref()
            .is_some_and(|s| s.contains("nesting limit reached")),
        "{:?}",
        facts.incomplete
    );
}

#[test]
fn ordinary_extraction_still_selects_its_supplied_defines() {
    let source =
        "class Worker { void Target(\n#if A\nint value\n#else\nstring value\n#endif\n) {} }";
    for (defines, expected) in [(vec![], "string"), (vec!["A".into()], "int")] {
        let facts = super::super::extract(source, &defines).unwrap();
        assert!(!facts.errors);
        assert!(facts.csharp.is_some());
        let parameters: Vec<_> = facts
            .declarations
            .iter()
            .filter(|d| d.name == "Target")
            .map(|d| d.parameters.clone())
            .collect();
        assert_eq!(parameters, [vec![expected.to_owned()]]);
    }
    assert_eq!(signatures(&extract(source).unwrap()).len(), 2);
}

#[test]
fn conditional_calls_are_not_merged_into_caller_or_wrapper_evidence() {
    let source = "class Worker { void Target() {\n#if A\nFirst();\n#else\nSecond();\n#endif\n} }";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none());
    assert!(facts.occurrences.is_empty());
    let direct = extract("class Worker { void Target() { First(); } }").unwrap();
    assert_eq!(direct.occurrences.len(), 1);
}

#[test]
fn shared_methods_do_not_erase_written_generic_owner_alternatives() {
    let source =
        "#if A\nclass Worker<T>\n#else\nclass Worker<T,U>\n#endif\n{ int Target() { return 1; } }";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    let methods: Vec<_> = facts
        .declarations
        .iter()
        .filter(|d| d.name == "Target")
        .collect();
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0].span, methods[1].span);
    assert_eq!(methods[0].parameters, methods[1].parameters);
    assert_ne!(methods[0].written, methods[1].written);
}

#[test]
fn unused_capacity_can_exhaust_the_fact_budget() {
    for facts in [
        HistoricalFacts {
            declarations: Vec::with_capacity(
                FACT_BYTES / std::mem::size_of::<HistoricalDeclaration>() + 1,
            ),
            ..HistoricalFacts::default()
        },
        HistoricalFacts {
            occurrences: Vec::with_capacity(
                FACT_BYTES / std::mem::size_of::<crate::model::Occurrence>() + 1,
            ),
            ..HistoricalFacts::default()
        },
    ] {
        assert!(facts.declarations.is_empty() && facts.occurrences.is_empty());
        assert!(fact_bytes(&facts) > FACT_BYTES);
    }
}

#[test]
fn an_unfinished_alternative_does_not_make_the_other_one_unique() {
    let source = "class Worker {\n#if A\nint Target(int value) { return value; }\n#else\nint Target(\n#endif\n}";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_some());
    assert!(facts.declarations.iter().any(|d| d.name == "Target"));
    assert!(facts.declarations.iter().all(|d| d.uncertain));
}

#[test]
fn guarded_membership_merges_common_headers_but_not_attributes() {
    let source = "class Worker {\n#if A\n[Marker]\n#endif\nint Target() { return 1; }\n}";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    let owner = facts
        .declarations
        .iter()
        .find(|d| d.kind == "class")
        .unwrap();
    assert_eq!(owner.guard, TRUE);
    let methods: Vec<_> = facts
        .declarations
        .iter()
        .filter(|d| d.name == "Target")
        .collect();
    assert_eq!(methods.len(), 2);
    assert_ne!(methods[0].span, methods[1].span);
    let mut coverage = facts.presence.coverage();
    coverage.add(methods[0].guard).unwrap();
    assert!(!coverage.uncovered(methods[0].guard).unwrap());
    assert!(coverage.uncovered(methods[1].guard).unwrap());
    coverage.add(methods[1].guard).unwrap();
    assert!(!coverage.uncovered(owner.guard).unwrap());
}

fn independent_methods(count: usize) -> String {
    let mut source = "class Worker {\nint Target() { return 1; }\n".to_owned();
    for index in 0..count {
        source.push_str(&format!(
            "int Other{index}(\n#if FEATURE_{index}\nint value\n#else\nstring value\n#endif\n) {{ return 0; }}\n"
        ));
    }
    source.push_str("}\n");
    source
}

#[test]
fn sparse_context_parses_preserve_tokens_spans_and_line_comments() {
    for gap in [" ", "\r\n  \r\n", "\u{2028}\t", "\n\n// 🦀\r\n"] {
        let source = format!(
            "class Café {{ int Before;{gap}// selected\nint Target() {{ return 1; }}{gap}int After; }}"
        );
        let mut conditions = Conditions::default();
        let document = Document::new(&source, &mut conditions).unwrap();
        let context = Context::whole(source.len());
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .unwrap();
        let seed = Work::default().parse(&mut parser, &source).unwrap();
        let mut inventory = contexts::Inventory::default();
        inventory.record(&seed).unwrap();
        for context in inventory
            .plan(source.len())
            .unwrap()
            .into_iter()
            .chain([context])
        {
            let projection = document
                .project(&mut conditions, &[], &context.ranges)
                .unwrap();
            let full = Work::default()
                .parse(&mut parser, &projection.text)
                .unwrap();
            let sparse = Work::default()
                .parse_ranges(
                    &mut parser,
                    &projection.text,
                    &document.included_ranges(&context.ranges),
                    None,
                )
                .unwrap();
            assert_eq!(sparse.root_node().has_error(), full.root_node().has_error());
            // Invalid speculative seams are discarded before their facts are
            // used. Recovery spans need not agree, but rejection must agree.
            if full.root_node().has_error() {
                continue;
            }
            assert_eq!(sparse.root_node().to_sexp(), full.root_node().to_sexp());
            let spans = |tree: &tree_sitter::Tree| {
                let mut result = Vec::new();
                let mut cursor = tree.walk();
                'walk: loop {
                    let node = cursor.node();
                    if node.child_count() == 0 {
                        result.push((
                            node.kind().to_owned(),
                            node.byte_range(),
                            node.start_position(),
                            node.end_position(),
                        ));
                    }
                    if cursor.goto_first_child() {
                        continue;
                    }
                    loop {
                        if cursor.goto_next_sibling() {
                            break;
                        }
                        if !cursor.goto_parent() {
                            break 'walk;
                        }
                    }
                }
                result
            };
            assert_eq!(spans(&sparse), spans(&full));
            assert_eq!(context.valid(&sparse), context.valid(&full));
        }
    }
}

#[test]
fn incremental_alternatives_match_fresh_trees_and_positions() {
    fn same_tree(actual: &tree_sitter::Tree, expected: &tree_sitter::Tree) {
        let mut a = actual.walk();
        let mut b = expected.walk();
        loop {
            let (left, right) = (a.node(), b.node());
            assert_eq!(
                (
                    left.kind(),
                    left.byte_range(),
                    left.start_position(),
                    left.end_position()
                ),
                (
                    right.kind(),
                    right.byte_range(),
                    right.start_position(),
                    right.end_position()
                ),
            );
            assert_eq!(left.has_error(), right.has_error());
            assert_eq!(left.is_missing(), right.is_missing());
            let descend = a.goto_first_child();
            assert_eq!(descend, b.goto_first_child());
            if descend {
                continue;
            }
            loop {
                let sibling = a.goto_next_sibling();
                assert_eq!(sibling, b.goto_next_sibling());
                if sibling {
                    break;
                }
                let ascend = a.goto_parent();
                assert_eq!(ascend, b.goto_parent());
                if !ascend {
                    return;
                }
            }
        }
    }
    for source in [
        "\u{feff}class Café {\r\nint Common() => 1;\r\n#if A\r\nstring Value => \"é🦀\";\r\n#else\r\nint Value => 2;\r\n#endif\r\n#if B\r\nvoid Extra() { Common(); }\r\n#endif\r\n}\r\n",
        "#if A\nnamespace One {\n#else\nnamespace Two {\n#endif\nclass Data {\n// comment\n#if B\nstring Text => @\"first\n#if literal\nend\";\n#else\nstring Text => \"other\";\n#endif\n}\n}\n",
    ] {
        let mut conditions = Conditions::default();
        let document = Document::new(source, &mut conditions).unwrap();
        let whole = Context::whole(source.len());
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .unwrap();
        let mut inventory = contexts::Inventory::default();
        let mut environments = Vec::new();
        let mut remaining = TRUE;
        while remaining != FALSE {
            let values = conditions.witness(remaining, false);
            let projection = document
                .project(&mut conditions, &values, &whole.ranges)
                .unwrap();
            let tree = Work::default()
                .parse(&mut parser, &projection.text)
                .unwrap();
            assert!(!tree.root_node().has_error());
            inventory.record(&tree).unwrap();
            environments.push(values);
            let other = conditions.not(projection.guard).unwrap();
            remaining = conditions.and(remaining, other).unwrap();
        }
        for context in inventory
            .plan(source.len())
            .unwrap()
            .into_iter()
            .chain([whole])
        {
            let ranges = document.included_ranges(&context.ranges);
            let mut previous: Option<(tree_sitter::Tree, Vec<bool>)> = None;
            for values in environments.iter().chain(environments.iter().rev()) {
                let projection = document
                    .project(&mut conditions, values, &context.ranges)
                    .unwrap();
                let fresh = Work::default()
                    .parse_ranges(&mut parser, &projection.text, &ranges, None)
                    .unwrap();
                let old = previous.as_mut().map(|(tree, before)| {
                    document.edit_tree(tree, &conditions, before, values, &context.ranges);
                    &*tree
                });
                let reused = Work::default()
                    .parse_ranges(&mut parser, &projection.text, &ranges, old)
                    .unwrap();
                same_tree(&reused, &fresh);
                previous = Some((reused, values.clone()));
            }
        }
    }
}

#[test]
fn independent_declaration_work_is_additive_at_the_symbol_limit() {
    for count in [16, 128] {
        let source = independent_methods(count);
        let mut work = Work::default();
        let facts = run(&source, &mut work).unwrap();
        assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
        assert_eq!(facts.declarations.len(), 2 * count + 2);
        assert!(facts.declarations.iter().all(|d| !d.uncertain));
        assert!(facts.occurrences.is_empty());
        for index in 0..count {
            let name = format!("Other{index}");
            let parameters: BTreeSet<_> = facts
                .declarations
                .iter()
                .filter(|d| d.name == name)
                .map(|d| d.parameters.clone())
                .collect();
            assert_eq!(
                parameters,
                BTreeSet::from([vec!["int".into()], vec!["string".into()]])
            );
        }
        assert_eq!(work.fallbacks, 0);
        assert_eq!(work.contexts, count + 2);
        assert_eq!(work.max_alternatives, 2);
        // Two discovery witnesses, one owner shell, one fixed Target and two
        // parses per Other. This measures work, not machine-dependent timing.
        assert!(work.parses <= 2 * count + 4, "{} parses", work.parses);
        // The parser visits each declaration and its owner shell, rather than
        // scanning a full file of blank sibling declarations on every pass.
        assert!(
            work.bytes < 8 * source.len(),
            "{} parsed bytes for {} source bytes",
            work.bytes,
            source.len()
        );
    }
}

#[test]
fn discovery_covers_optional_declarations_absent_from_the_seed() {
    let count = 16;
    let mut source = "class Worker {\nint Target() { return 1; }\n".to_owned();
    for index in 0..count {
        source.push_str(&format!(
            "#if FEATURE_{index}\nvoid Other{index}() {{}}\n#endif\n"
        ));
    }
    source.push_str("}\n");
    let mut work = Work::default();
    let facts = run(&source, &mut work).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(facts.declarations.len(), count + 2);
    assert_eq!(work.fallbacks, 0);
    assert_eq!(work.max_alternatives, 2);
    assert!(work.parses <= 2 * count + 4);
}

#[test]
fn independent_owner_headers_do_not_multiply_root_contexts() {
    let count = 20;
    let mut source = String::new();
    for index in 0..count {
        source.push_str(&format!(
            "#if OWNER_{index}\nclass Owner{index}<T>\n#else\nclass Owner{index}<T,U>\n#endif\n{{ int Target() {{ return 1; }} }}\n"
        ));
    }
    let mut work = Work::default();
    let facts = run(&source, &mut work).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(facts.declarations.len(), 4 * count);
    assert_eq!(work.fallbacks, 0);
    assert_eq!(work.max_alternatives, 2);
    assert!(work.parses <= 4 * count + 2);
    for index in 0..count {
        let qualified = format!("Owner{index}.Target");
        assert_eq!(
            facts
                .declarations
                .iter()
                .filter(|d| d.qualified == qualified)
                .map(|d| &d.written)
                .collect::<BTreeSet<_>>()
                .len(),
            2
        );
    }
}

#[test]
fn local_definitions_join_without_losing_owner_and_member_correlations() {
    let source = independent_methods(16).replace(
        "class Worker {\nint Target() { return 1; }",
        "#if OWNER\n#define MEMBER\n#else\n#undef MEMBER\n#endif\n#if OWNER\nclass First<T>\n#else\nclass Second<T,U>\n#endif\n{\n#if MEMBER\nint\n#else\nstring\n#endif\nTarget(\n#if OWNER\nint value\n#else\nstring value\n#endif\n) { return default; }",
    );
    let mut work = Work::default();
    let facts = run(&source, &mut work).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    let methods: BTreeSet<_> = facts
        .declarations
        .iter()
        .filter(|d| d.name == "Target")
        .map(|d| (d.qualified.clone(), d.ty.clone(), d.parameters.clone()))
        .collect();
    assert_eq!(
        methods,
        BTreeSet::from([
            ("First.Target".into(), "int".into(), vec!["int".into()]),
            (
                "Second.Target".into(),
                "string".into(),
                vec!["string".into()]
            ),
        ])
    );
    assert_eq!(work.fallbacks, 0);
    assert_eq!(work.max_alternatives, 4);
}

#[test]
fn independent_conditional_symbol_assignments_join_as_functions() {
    let mut source = String::new();
    for index in 0..16 {
        source.push_str(&format!(
            "#if INPUT_{index}\n#define FEATURE_{index}\n#else\n#undef FEATURE_{index}\n#endif\n"
        ));
    }
    source.push_str(&independent_methods(16));
    let mut work = Work::default();
    let facts = run(&source, &mut work).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(facts.declarations.len(), 34);
    assert_eq!(work.fallbacks, 0);
    assert_eq!(work.max_alternatives, 2);
    assert!(work.parses <= 36);
}

#[test]
fn conditional_undef_preserves_the_unknown_initial_symbol_elsewhere() {
    let source = "#if A\n#undef B\n#endif\nclass Worker {\n#if A && B\nvoid Never() {}\n#endif\nvoid Target(\n#if B\nint value\n#else\nstring value\n#endif\n) {} }";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(signatures(&facts).len(), 2);
    assert!(!facts.declarations.iter().any(|d| d.name == "Never"));
}

#[test]
fn optional_literal_directive_text_can_have_different_scanner_stacks() {
    let source = "#if FEATURE\nclass Worker {\nconst string Banner = \"\"\"\n#if LITERAL\n#endif\n\"\"\";\nint Target() { return 1; }\n}\n#endif\n";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(signatures(&facts), BTreeSet::from([("int".into(), vec![])]));
}

#[test]
fn a_real_header_explosion_remains_bounded_after_factoring() {
    let mut source = "class Worker { void Target(int seed\n".to_owned();
    for index in 0..16 {
        source.push_str(&format!("#if A{index}\n, int p{index}\n#endif\n"));
    }
    source.push_str(") {} }");
    let mut work = Work::default();
    let facts = run(&source, &mut work).unwrap();
    assert!(
        facts
            .incomplete
            .as_deref()
            .unwrap()
            .contains("limit reached")
    );
    assert!(facts.declarations.iter().all(|d| d.uncertain));
    assert_eq!(work.max_alternatives, PROJECTIONS);
    assert!(work.parses <= PROJECTIONS + 3);
    assert_eq!(work.fallbacks, 0);
}

#[test]
fn parse_bytes_are_global_across_discovery_and_contexts() {
    let source = independent_methods(16);
    let mut work = Work {
        bytes: PARSE_BYTES - source.len(),
        ..Work::default()
    };
    let facts = run(&source, &mut work).unwrap();
    assert_eq!(work.parses, 1);
    assert_eq!(work.bytes, PARSE_BYTES);
    assert!(
        facts
            .incomplete
            .as_deref()
            .unwrap()
            .contains("parse byte limit reached")
    );
    assert!(facts.declarations.iter().all(|d| d.uncertain));
    assert!(facts.occurrences.is_empty());
}

#[test]
fn shared_build_conditions_do_not_reparse_every_declaration() {
    let mut source = String::from("class Worker {\n");
    for index in 0..64 {
        source.push_str(&format!(
            "#if EDITOR\nint Method{index}(int value) {{ return value + {index}; }}\n#else\nstring Method{index}(string value) {{ return value; }}\n#endif\n"
        ));
    }
    source.push_str("}\n");
    let mut work = Work::default();
    let facts = run(&source, &mut work).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(facts.declarations.len(), 129);
    assert!(work.parses < 8, "Repeated parsing per declaration");
    let whole = exhaustive(&source);
    for offset in 0..source.len() {
        assert_eq!(positional(&facts, offset), positional(&whole, offset));
    }
}

#[test]
fn changing_list_boundaries_use_verified_whole_source_fallback() {
    let source = "class Worker {\n#if A\nvoid Before() {}\n#else\nvoid Before() {\n#endif\nvoid Target() {}\n#if !A\n}\n#endif\n}\n#if B\nclass Other {}\n#endif\n";
    let mut work = Work::default();
    let facts = run(source, &mut work).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(work.fallbacks, 1);
    let names: BTreeSet<_> = facts
        .declarations
        .iter()
        .filter(|d| d.name == "Target")
        .map(|d| d.qualified.as_str())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["Worker.Target", "Worker.Before.Target"])
    );
}

fn exhaustive(source: &str) -> HistoricalFacts {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .unwrap();
    let mut conditions = Conditions::default();
    let document = Document::new(source, &mut conditions).unwrap();
    let mut facts = HistoricalFacts::default();
    explore_context(
        &document,
        &Context::whole(source.len()),
        false,
        &mut parser,
        &mut conditions,
        &mut facts,
        &mut Work::default(),
    )
    .unwrap();
    normalize(&mut facts.declarations, &mut conditions).unwrap();
    facts.presence = conditions.finish();
    facts
}

fn positional(facts: &HistoricalFacts, offset: usize) -> BTreeSet<(String, String)> {
    let mut declarations: Vec<_> = facts
        .declarations
        .iter()
        .filter(|d| d.span.contains(&offset))
        .collect();
    declarations.sort_by_key(|d| d.span.len());
    let mut coverage = facts.presence.coverage();
    let mut selected = BTreeSet::new();
    for group in declarations.chunk_by(|a, b| a.span.len() == b.span.len()) {
        for declaration in group {
            if coverage.uncovered(declaration.guard).unwrap() {
                selected.insert((declaration.qualified.clone(), declaration.written.clone()));
            }
        }
        for declaration in group {
            coverage.add(declaration.guard).unwrap();
        }
    }
    selected
}

#[test]
fn factored_facts_and_positions_agree_with_exhaustive_small_sources() {
    for source in [
        independent_methods(3),
        independent_methods(2).replace(
            "int Target() { return 1; }",
            "void Target() { var f =\n#if ARG\n(int value)\n#else\n(string value)\n#endif\n=> value; }",
        ),
        independent_methods(2).replace(
            "int Target() { return 1; }",
            "#if TYPE\nint\n#else\nstring\n#endif\nValue { get { return default; } set {} }",
        ),
        independent_methods(2).replace(
            "int Target() { return 1; }",
            "#if ATTRIBUTE\n[Marker]\n#endif\nSystem.Func<int> Field = () => 1;",
        ),
        independent_methods(2).replace(
            "int Target() { return 1; }",
            "void Target() {\n#if OPTIONAL\nvar f = () => { return 1; };\n#endif\n}",
        ),
        independent_methods(2).replace(
            "int Target() { return 1; }",
            "#if OWNER\nvoid Target(int value)\n#else\nvoid Target(string value)\n#endif\n{ var f = () => delegate { return value; }; }",
        ),
        independent_methods(2).replace(
            "int Target()",
            "#if ATTRIBUTE\n[Marker]\n#endif\nint Target()",
        ),
        independent_methods(2).replace(
            "class Worker",
            "#if OWNER\nclass Worker<T>\n#else\nclass Worker<T,U>\n#endif\n",
        ),
        independent_methods(2).replace(
            "int Target() { return 1; }",
            "#if OPTIONAL\nint Target() { return 1; }\n#endif",
        ),
        format!(
            "#if OWNER\nnamespace First;\n#else\nnamespace Second;\n#endif\n{}",
            independent_methods(2)
        ),
    ] {
        let factored = extract(&source).unwrap();
        let whole = exhaustive(&source);
        assert!(
            factored.incomplete.is_none(),
            "{:?}\n{source}",
            factored.incomplete
        );
        assert!(whole.incomplete.is_none());
        let headers = |facts: &HistoricalFacts| -> BTreeSet<_> {
            facts
                .declarations
                .iter()
                .map(|d| {
                    (
                        d.span.start,
                        d.span.end,
                        d.header.start,
                        d.header.end,
                        d.qualified.clone(),
                        d.kind.clone(),
                        d.parameters.clone(),
                        d.written.clone(),
                    )
                })
                .collect()
        };
        assert_eq!(headers(&factored), headers(&whole), "{source}");
        // Byte-for-byte positional selection also tests conditional absence and
        // owner fallback, not just matching named overloads at their name tokens.
        for offset in 0..source.len() {
            assert_eq!(
                positional(&factored, offset),
                positional(&whole, offset),
                "offset {offset}\n{source}"
            );
        }
    }
}

#[test]
fn anonymous_forms_keep_original_spans_and_leave_snapshot_binding_alone() {
    for (expression, body) in [
        ("value => value + 1", "value + 1"),
        (
            "(int value) => { return value + 1; }",
            "{ return value + 1; }",
        ),
        (
            "static async (int value) => await Load(value)",
            "await Load(value)",
        ),
        (
            "delegate(int value) { return value + 1; }",
            "{ return value + 1; }",
        ),
        ("async delegate { await Load(1); }", "{ await Load(1); }"),
        ("() => /* café */ \"żółw\".Length", "\"żółw\".Length"),
    ] {
        let source = format!(
            "\u{feff}// 🦀\r\nclass Worker {{ void Run() {{ var target = {expression}; }} }}\r\n"
        );
        let facts = extract(&source).unwrap();
        assert!(
            facts.incomplete.is_none(),
            "{:?}: {source}",
            facts.incomplete
        );
        let closures: Vec<_> = facts
            .declarations
            .iter()
            .filter(|d| d.kind == "closure")
            .collect();
        assert_eq!(closures.len(), 1, "{source}");
        let closure = closures[0];
        assert_eq!(&source[closure.span.clone()], expression);
        assert_eq!(&source[closure.header.end..closure.span.end], body);
        assert_eq!(closure.owner, "Worker.Run");
        assert!(!closure.written.contains("café"));
        assert!(!closure.uncertain);
        let ordinary = super::super::extract(&source, &[]).unwrap();
        assert!(!ordinary.errors);
        assert!(ordinary.csharp.is_some());
        assert!(!ordinary.declarations.iter().any(|d| d.kind == "closure"));
    }
}

#[test]
fn accessor_regions_cover_properties_indexers_events_and_auto_accessors() {
    for (member, owner, accessors) in [
        (
            "int Value { get { return backing; } set { backing = value; } }",
            "Worker.Value",
            [
                ("get", "get { return backing; }", "{ return backing; }"),
                ("set", "set { backing = value; }", "{ backing = value; }"),
            ],
        ),
        (
            "int Value { [Marker] get => backing; private init => backing = value; }",
            "Worker.Value",
            [
                ("get", "[Marker] get => backing;", "=> backing;"),
                (
                    "init",
                    "private init => backing = value;",
                    "=> backing = value;",
                ),
            ],
        ),
        (
            "int this[int index] { get => index; set => Save(index, value); }",
            "Worker.Item",
            [
                ("get", "get => index;", "=> index;"),
                (
                    "set",
                    "set => Save(index, value);",
                    "=> Save(index, value);",
                ),
            ],
        ),
        (
            "event System.Action Changed { add => Save(value); remove { Save(value); } }",
            "Worker.Changed",
            [
                ("add", "add => Save(value);", "=> Save(value);"),
                ("remove", "remove { Save(value); }", "{ Save(value); }"),
            ],
        ),
        (
            "int Value { get; init; }",
            "Worker.Value",
            [("get", "get;", ""), ("init", "init;", "")],
        ),
    ] {
        let source = format!("class Worker {{ {member} }}");
        let facts = extract(&source).unwrap();
        assert!(
            facts.incomplete.is_none(),
            "{:?}: {source}",
            facts.incomplete
        );
        let regions: Vec<_> = facts
            .declarations
            .iter()
            .filter(|d| d.kind == "accessor")
            .collect();
        assert_eq!(regions.len(), accessors.len());
        for (name, written, body) in accessors {
            let region = regions.iter().find(|d| d.name == name).unwrap();
            assert_eq!(region.owner, owner);
            if owner == "Worker.Item" {
                assert_eq!(region.parameters, ["int"]);
            }
            assert_eq!(&source[region.span.clone()], written);
            assert_eq!(&source[region.header.end..region.span.end], body);
        }
        let ordinary = super::super::extract(&source, &[]).unwrap();
        assert!(!ordinary.errors);
        assert!(!ordinary.declarations.iter().any(|d| d.kind == "accessor"));
        assert!(
            ordinary
                .declarations
                .iter()
                .any(|d| d.kind == "parameter" && d.name == "value")
        );
    }
}

#[test]
fn field_initializer_callables_do_not_inherit_sibling_bodies_or_fake_local_fields() {
    let source = "class Worker<T> { System.Func<int> first = () => 7, target = () => { var adjusted = 1; return adjusted; }; }";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none());
    assert!(!facts.declarations.iter().any(|d| d.name == "adjusted"));
    let target = facts
        .declarations
        .iter()
        .find(|d| d.kind == "closure" && d.owner == "Worker.target")
        .unwrap();
    assert_eq!(
        target.written,
        "class Worker < T > / System . Func < int > target / ( ) =>"
    );
    let selected = positional(&facts, source.find("var adjusted").unwrap());
    assert_eq!(selected.len(), 1);
    assert_eq!(selected.first().unwrap().0, target.qualified);
    let changed = extract(&source.replace("() => 7", "() => 900")).unwrap();
    let previous = changed
        .declarations
        .iter()
        .find(|d| d.qualified == target.qualified)
        .unwrap();
    assert_eq!(previous.written, target.written);
}

#[test]
fn nested_callables_keep_lexical_owners_without_ordinal_names() {
    let source =
        "class Worker { void Run() { var a = x => y => x + y; var b = x => y => x - y; } }";
    let facts = extract(source).unwrap();
    assert!(facts.incomplete.is_none());
    let closures: Vec<_> = facts
        .declarations
        .iter()
        .filter(|d| d.kind == "closure")
        .collect();
    assert_eq!(closures.len(), 4);
    assert_eq!(closures[0].qualified, closures[2].qualified);
    assert_eq!(closures[1].owner, closures[0].qualified);
    let selected = positional(&facts, source.find("x + y").unwrap());
    assert_eq!(selected.len(), 1);
    assert_eq!(selected.first().unwrap().0, "Worker.Run.<lambda>.<lambda>");
}

#[test]
fn independent_callable_headers_remain_factored_and_bounded() {
    let mut source = "class Worker {\n".to_owned();
    for index in 0..16 {
        source.push_str(&format!(
            "void Other{index}() {{ var f =\n#if FEATURE_{index}\n(int value)\n#else\n(string value)\n#endif\n=> value; }}\n"
        ));
    }
    source.push('}');
    let mut work = Work::default();
    let facts = run(&source, &mut work).unwrap();
    assert!(facts.incomplete.is_none(), "{:?}", facts.incomplete);
    assert_eq!(
        facts
            .declarations
            .iter()
            .filter(|d| d.kind == "closure")
            .count(),
        32
    );
    assert_eq!(work.fallbacks, 0);
    assert_eq!(work.max_alternatives, 2);
    assert!(work.parses <= 36);
}
