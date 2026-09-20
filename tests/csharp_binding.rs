use sigla::{discovery::Policy, service::App};
use std::sync::Arc;

#[tokio::test]
async fn implicit_iteration_await_and_disposal_calls_are_retrievable() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Test.csproj"),
        "<Project><ItemGroup><Compile Include=\"Test.cs\"/></ItemGroup></Project>",
    )
    .unwrap();
    std::fs::write(
        root.path().join("Test.cs"),
        r#"
class Item {}
class Enumerator { public Item Current => null; public bool MoveNext() => false; }
class Items { public Enumerator GetEnumerator() => null; }
class Awaiter { public Item GetResult() => null; }
class Awaitable { public Awaiter GetAwaiter() => null; }
class Resource { public void Dispose() {} }
class Usage { async void Execute(Items items, Awaitable pending, Resource resource) {
 foreach (var item in items) {}
 var result = await pending;
 using (resource) {}
} }
"#,
    )
    .unwrap();
    let app = Arc::new(
        App::new(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            2,
        )
        .unwrap(),
    );
    for target in [
        "Enumerator.MoveNext",
        "Awaiter.GetResult",
        "Resource.Dispose",
    ] {
        let result = app
            .search(root.path().to_str().unwrap(), &format!("calls:{target}"))
            .await
            .unwrap();
        assert!(result.contains("Usage.Execute"), "{target}: {result}");
        assert!(!result.contains("Possible"), "{result}");
    }
}

#[tokio::test]
async fn local_functions_use_lexical_parameters_and_generic_inference() {
    check(
        r#"
class Item { public void Run() {} }
class Task {
 T Identity<T>(T value) => value;
 T Forward<T>(T value) => /*open-parameter*/Identity(value);
 void Execute(Item item) {
 T Identity<T>(T value) => value;
 Identity(item)./*local-function*/Run();
} }
"#,
        &[
            ("/*local-function*/", "Item.Run"),
            ("/*open-parameter*/", "Task.Identity"),
        ],
    )
    .await;
}

#[tokio::test]
async fn partial_bases_keep_their_own_import_context() {
    check_extra(r#"
partial class Split<T> { }
class Item { public void Run() {} }
class Task { void Execute(Split<Item> split) { split.Value./*partial-base*/Run(); } }
"#, "using Parent = Library; namespace Library { class Base<T> { public T Value => default; } } partial class Split<T> : Parent.Base<T> {}", &[("/*partial-base*/", "Item.Run")]).await;
}

#[tokio::test]
async fn containing_type_arity_keeps_member_families_separate() {
    check(r#"
class First { public void Run() {} }
class Second { public void Run() {} }
class Owner { public First Value => null; public class Nested { public First Value => null; } }
class Owner<T> { public T Value => default; public class Nested { public T Value => default; } }
class Task { void Execute(Owner plain, Owner<Second> generic, Owner<Second>.Nested nested) {
 plain.Value./*plain-owner*/Run(); generic.Value./*generic-owner*/Run(); nested.Value./*nested-owner*/Run();
} }
"#, &[("/*plain-owner*/", "First.Run"), ("/*generic-owner*/", "Second.Run"), ("/*nested-owner*/", "Second.Run")]).await;
}

#[tokio::test]
async fn nearer_extension_scope_wins_before_outer_imports() {
    check(r#"
using Imported;
class InnerResult { public void Run() {} }
class OuterResult { public void Run() {} }
class Item {}
namespace Imported { static class Extensions { public static OuterResult Make(this Item item) => null; } }
namespace Outer {
 static class Extensions { public static OuterResult Make(this Item item) => null; }
 namespace Inner {
  static class Extensions { public static InnerResult Make(this Item item) => null; }
  class Task { void Execute(Item item) { item.Make()./*nearest-extension*/Run(); } }
 }
}
"#, &[("/*nearest-extension*/", "InnerResult.Run")]).await;
}

#[tokio::test]
async fn conditional_values_are_lifted_before_overload_selection() {
    check(
        r#"
class Plain { public void Run() {} }
class Lifted { public void Run() {} }
class Item { public int Count => 0; public int CountItems() => 0; }
class Task {
 Plain Pick(int count) => null;
 Lifted Pick(int? count) => null;
 void Execute(Item item) {
  Pick(item?.Count)./*conditional-property*/Run();
  Pick(item?.CountItems())./*conditional-method*/Run();
 }
}
"#,
        &[
            ("/*conditional-property*/", "Lifted.Run"),
            ("/*conditional-method*/", "Lifted.Run"),
        ],
    )
    .await;
}

#[tokio::test]
async fn inference_projects_interfaces_and_combines_argument_bounds() {
    check(
        r#"
class Base { public void Run() {} }
class Derived : Base {}
interface I<out T> {}
class Bag<T> : I<T> {}
class Task {
 T Merge<T>(T left, T right) => left;
 T First<T>(I<T> bag) => default;
 T Pack<T>(params T[] values) => default;
 void Execute(Derived derived, Base basis, Bag<Derived> bag, Derived[] array) {
  Merge(derived, basis)./*lower-bounds*/Run();
  First(bag)./*interface-projection*/Run();
  Pack(array)./*params-array*/Run();
  Pack(derived, basis)./*params-expanded*/Run();
 }
}
"#,
        &[
            ("/*lower-bounds*/", "Base.Run"),
            ("/*interface-projection*/", "Base.Run"),
            ("/*params-array*/", "Base.Run"),
            ("/*params-expanded*/", "Base.Run"),
        ],
    )
    .await;
}

#[tokio::test]
async fn named_argument_order_and_ref_identity_are_checked() {
    check(
        r#"
class Item { public void Run() {} }
class Task {
 Item Choose(int first, string second) => null;
 Item Reference(ref object value) => null;
 void Execute(string text) {
  Choose(first: 1, "ok")./*named-in-order*/Run();
  /*named-out-of-order*/Choose(second: "bad", 1);
  /*ref-conversion*/Reference(ref text);
 }
}
"#,
        &[
            ("/*named-in-order*/", "Item.Run"),
            ("/*named-out-of-order*/", "No matches."),
            ("/*ref-conversion*/", "No matches."),
        ],
    )
    .await;
}

#[tokio::test]
async fn constructed_interfaces_and_overrides_use_substituted_signatures() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Test.csproj"),
        "<Project><ItemGroup><Compile Include=\"Test.cs\" /></ItemGroup></Project>",
    )
    .unwrap();
    std::fs::write(
        root.path().join("Test.cs"),
        r#"
class Item {}
interface I<T> { void Use(T value); }
class Implicit : I<Item> { public void Use(Item value) {} }
class Explicit : I<Item> { void I<Item>.Use(Item value) {} }
class Wrong : I<Item> { public void Use(string value) {} }
class Base<T> { public virtual void Use(T value) {} }
class Override : Base<Item> { public override void Use(Item value) {} }
class Hidden : Base<Item> { public new void Use(Item value) {} }
"#,
    )
    .unwrap();
    let app = Arc::new(
        App::new(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            2,
        )
        .unwrap(),
    );
    let interface = app
        .search(root.path().to_str().unwrap(), "impl:I.Use")
        .await
        .unwrap();
    assert!(interface.contains("Implicit.Use"), "{interface}");
    assert!(interface.contains("Explicit.Use"), "{interface}");
    assert!(!interface.contains("Wrong.Use"), "{interface}");
    let overrides = app
        .search(root.path().to_str().unwrap(), "impl:Base.Use")
        .await
        .unwrap();
    assert!(overrides.contains("Override.Use"), "{overrides}");
    assert!(!overrides.contains("Hidden.Use"), "{overrides}");
}

#[tokio::test]
async fn method_groups_supply_delegate_result_inference() {
    check(
        r#"
delegate R Func<T,R>(T value);
class Item { public void Run() {} }
class Input {}
class Task {
 Item Convert(Input input) => null;
 R Apply<T,R>(T input, Func<T,R> convert) => convert(input);
 R Create<T,R>(Func<T,R> convert) => default;
 void Execute(Input input) { var result = Apply(input, Convert); result./*method-group*/Run(); }
 void TypedLambda() { Create((Input input) => Convert(input))./*typed-lambda*/Run(); }
}
"#,
        &[
            ("/*method-group*/", "Item.Run"),
            ("/*typed-lambda*/", "Item.Run"),
        ],
    )
    .await;
}

#[tokio::test]
async fn indexers_patterns_out_variables_and_contextual_new_keep_declared_types() {
    check(
        r#"
class Item { public void Run() {} }
class Bag { public Item this[int index] => null; }
class Task {
 bool TryGet(out Item item) { item = null; return true; }
 Item Accept(Item item) => item;
 void Execute(Bag bag, object value, Item[] array) {
  bag/*indexer*/[0]./*indexed-item*/Run();
  array[0]./*array-item*/Run();
  if (value is Item matched) matched./*pattern*/Run();
  TryGet(out var found);
  found./*out-variable*/Run();
  Accept(new())./*target-new*/Run();
  found?./*conditional*/Run();
 }
}
"#,
        &[
            ("/*indexer*/", "Bag.Item"),
            ("/*indexed-item*/", "Item.Run"),
            ("/*array-item*/", "Item.Run"),
            ("/*pattern*/", "Item.Run"),
            ("/*out-variable*/", "Item.Run"),
            ("/*target-new*/", "Item.Run"),
            ("/*conditional*/", "Item.Run"),
        ],
    )
    .await;
}

#[tokio::test]
async fn global_static_imports_and_namespace_aliases_keep_project_scope() {
    check_extra(r#"
using Alias = Library;
namespace Library { public class Item { public void Run() {} } public static class Factory { public static Item Make() => null; } }
class Task {
 void Execute() {
  var item = /*static-import*/Make();
  item./*global-type*/Run();
  Alias.Item another = new();
  another./*namespace-alias*/Run();
 }
}
"#, "global using Library; global using static Library.Factory;", &[("/*static-import*/", "Library.Factory.Make"), ("/*global-type*/", "Library.Item.Run"), ("/*namespace-alias*/", "Library.Item.Run")]).await;
}

#[tokio::test]
async fn unresolved_competitors_and_dynamic_do_not_fabricate_a_target() {
    check(
        r#"
class Methods {
 public Item Choose(object value) => null;
 public Item Choose(MissingType value) => null;
 public static T Create<T>() => default;
 void Run(Methods receiver, dynamic unknown) {
  receiver./*unknown-competitor*/Choose("value");
  unknown./*dynamic*/Choose("value");
  string destination = /*no-return-inference*/Create();
  receiver.Choose("value")./*unknown-chain*/Run();
 }
}
class Item { public void Run() {} }
"#,
        &[
            ("/*unknown-competitor*/", "No matches."),
            ("/*dynamic*/", "No matches."),
            ("/*no-return-inference*/", "No matches."),
            ("/*unknown-chain*/", "No matches."),
        ],
    )
    .await;
}

async fn check(source: &str, cases: &[(&str, &str)]) {
    check_extra(source, "", cases).await;
}

async fn check_extra(source: &str, extra: &str, cases: &[(&str, &str)]) {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Test.csproj"),
        "<Project><ItemGroup><Compile Include=\"Test.cs\" /><Compile Include=\"Global.cs\" /></ItemGroup></Project>",
    )
    .unwrap();
    std::fs::write(root.path().join("Test.cs"), source).unwrap();
    std::fs::write(root.path().join("Global.cs"), extra).unwrap();
    let app = Arc::new(
        App::new(
            Policy::new(vec![root.path().into()]).unwrap(),
            cache.path().into(),
            2,
        )
        .unwrap(),
    );
    for (marker, expected) in cases {
        let position = source.find(marker).unwrap() + marker.len();
        let (line, column) = sigla::model::position(source, position);
        let result = app
            .search(
                root.path().to_str().unwrap(),
                &format!("@Test.cs:{line}:{column}"),
            )
            .await
            .unwrap();
        assert!(
            result.contains(expected),
            "{marker}: expected {expected}, got {result}"
        );
    }
}

#[tokio::test]
async fn constructed_property_and_generic_base_preserve_static_type() {
    check(
        r#"
class Singleton<T> { public static T Instance { get; } }
class Pool { public void Initialize() {} }
class Base<T> { public T Value { get; } public virtual void Run() {} }
class Derived : Base<Pool> { public override void Run() {} }
class Tasks {
 void Execute() {
  Singleton<Pool>.Instance./*singleton*/Initialize();
  var value = new Derived();
  value.Value./*base-property*/Initialize();
  Base<Pool> declared = new Derived();
  declared./*static-type*/Run();
 }
}
"#,
        &[
            ("/*singleton*/", "Pool.Initialize"),
            ("/*base-property*/", "Pool.Initialize"),
            ("/*static-type*/", "Base.Run"),
        ],
    )
    .await;
}

#[tokio::test]
async fn optional_named_arguments_and_overloads_select_by_type() {
    check(
        r#"
class Methods {
 public int Parse(int value, bool enabled = true) => value;
 public string Parse(string value) => value;
 void Run(Methods receiver) {
  receiver./*number*/Parse(enabled: false, value: 1);
  receiver./*text*/Parse("value");
 }
}
"#,
        &[
            ("/*number*/", "int Parse(int"),
            ("/*text*/", "string Parse(string"),
        ],
    )
    .await;
}

#[tokio::test]
async fn extensions_foreach_and_lambdas_share_constructed_signatures() {
    check(r#"
delegate R Func<T,R>(T item);
class Sequence<T> { public Iterator<T> GetEnumerator() => null; }
class Iterator<T> { public T Current { get; } public bool MoveNext() => true; }
class Enemy { public bool IsAlive { get; } public Transform Transform { get; } }
class Transform { public void Move() {} }
interface IComponent { void Register(); }
class GameObject {}
static class Extensions {
 public static Sequence<T> Enumerate<T>(this GameObject value, bool inactive) => null;
 public static Sequence<T> Where<T>(this Sequence<T> items, Func<T,bool> predicate) => items;
 public static Sequence<R> Select<T,R>(this Sequence<T> items, Func<T,R> selector) => null;
}
class Tasks {
 void Run(GameObject obj, Sequence<Enemy> enemies) {
  foreach (var item in obj.Enumerate<IComponent>(true)) item./*foreach*/Register();
  var selected = enemies.Where(enemy => enemy./*lambda-in*/IsAlive).Select(enemy => enemy./*lambda-out*/Transform);
  foreach (var item in selected) item./*inferred-result*/Move();
 }
}
"#, &[("/*foreach*/", "IComponent.Register"), ("/*lambda-in*/", "Enemy.IsAlive"), ("/*lambda-out*/", "Enemy.Transform"), ("/*inferred-result*/", "Transform.Move")]).await;
}
