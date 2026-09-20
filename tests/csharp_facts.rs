use sigla::{
    csharp::{syntax::*, types::*},
    extract,
    model::Language,
};

#[test]
fn structured_source_retains_signatures_imports_and_owned_expressions() {
    let source = r#"
global using static Tools;
using Alias = Outer<int>.Inner<string>;
class Outer<T> {
    public class Inner<U> {
        public T[,] Map<V>(ref U input, int count = 1, params V[] values) where V : class, new() => null;
    }
}
static class Tools {
    public static T Echo<T>(this T value) => value;
    void Run(Alias input) { var result = input.Map<string>(ref item, count: 2); }
}
"#;
    let facts = extract::extract(source, Language::CSharp, &[], "").unwrap();
    assert!(!facts.errors);
    let syntax = facts.csharp.unwrap();
    assert!(matches!(syntax.imports[0].kind, ImportKind::Static));
    assert!(syntax.imports[0].global);
    assert!(matches!(&syntax.imports[1].kind, ImportKind::Alias(alias) if alias == "Alias"));
    let index = facts
        .declarations
        .iter()
        .position(|d| d.name == "Map")
        .unwrap();
    let header = &syntax.headers[index];
    assert!(matches!(header.ty, WrittenType::Array(_, 2)));
    assert_eq!(header.parameters[0].mode, PassingMode::Ref);
    assert!(header.parameters[1].default.is_some());
    assert!(header.parameters[2].variadic);
    assert!(
        header.generics[0]
            .special_constraints
            .iter()
            .any(|c| c == "class")
    );
    let echo = facts
        .declarations
        .iter()
        .position(|d| d.name == "Echo")
        .unwrap();
    assert!(syntax.headers[echo].parameters[0].receiver);
    let local = syntax.locals.iter().find(|l| l.name == "result").unwrap();
    let call = &syntax.expressions[local.value.unwrap() as usize];
    let ExpressionKind::Call { arguments, .. } = &call.kind else {
        panic!("{call:?}")
    };
    assert_eq!(arguments[0].mode, PassingMode::Ref);
    assert_eq!(arguments[1].name.as_deref(), Some("count"));
    assert!(source[call.span.clone()].contains("Map<string>"));
}

#[test]
fn lowering_retains_lambda_and_iteration_dependencies() {
    let source = include_str!("csharp-fixtures/Chains.cs");
    let facts = extract::extract(source, Language::CSharp, &[], "").unwrap();
    let syntax = facts.csharp.unwrap();
    let item = syntax.locals.iter().find(|l| l.iteration).unwrap();
    assert!(matches!(
        syntax.expressions[item.value.unwrap() as usize].kind,
        ExpressionKind::Call { .. }
    ));
    assert!(syntax.expressions.iter().any(|e| matches!(&e.kind, ExpressionKind::Lambda { parameters, .. } if parameters.len() == 1 && parameters[0].name == "enemy")));
    // Every child is earlier in the arena, making syntax traversal acyclic.
    for (index, expr) in syntax.expressions.iter().enumerate() {
        match &expr.kind {
            ExpressionKind::Member { receiver, name, .. } => {
                assert!((*receiver as usize) < index && (*name as usize) < index)
            }
            ExpressionKind::Call {
                function,
                arguments,
            } => {
                assert!((*function as usize) < index);
                assert!(arguments.iter().all(|a| (a.value as usize) < index));
            }
            _ => {}
        }
    }
}
