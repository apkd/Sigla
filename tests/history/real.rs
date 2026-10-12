//! Pinned public source snapshots; provenance is in fixtures/SOURCES.md.
use super::{Fixture, ids};

const CHECKER: &str = "src/checkstyle/com/puppycrawl/tools/checkstyle/Checker.java";
const COMMON: &str = "src/main/java/com/puppycrawl/tools/checkstyle/utils/CommonUtils.java";
const NUMBERS: &str = "src/main/java/org/apache/commons/lang3/math/NumberUtils.java";
const RUNNER: &str = "src/main/java/org/junit/runners/BlockJUnit4ClassRunner.java";

fn method_lines(source: &str, signature: &str) -> (usize, usize) {
    let at = source.find(signature).unwrap();
    let line = source[..at].rfind('\n').map_or(0, |p| p + 1);
    let indent = &source[line..at];
    let close = source[at..].find(&format!("\n{indent}}}")).unwrap();
    let first = source[..at].bytes().filter(|b| *b == b'\n').count() + 1;
    let last = first
        + source[at..at + close]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
        + 1;
    (first, last)
}

fn anchor(path: &str, source: &str, signature: &str) -> String {
    let (first, last) = method_lines(source, signature);
    format!("@{path}:{first}-{last}")
}

async fn whole_predecessor(
    fixture: &Fixture,
    child: &str,
    parent: &str,
    selected: &str,
    source: &str,
    signature: &str,
) {
    let (first, last) = method_lines(source, signature);
    let expected: String = source
        .split_inclusive('\n')
        .skip(first - 1)
        .take(last - first + 1)
        .collect();
    let shown = fixture
        .history(child, &format!("show {parent} {selected}"))
        .await;
    let added: String = shown
        .split_inclusive('\n')
        .filter(|line| !line.starts_with("+++ "))
        .filter_map(|line| line.strip_prefix('+'))
        .collect();
    // A named C# declaration starts at its token; a line selection also owns
    // the leading indentation. Both must retain the complete implementation.
    assert!(added.contains(expected.trim_start()), "{shown}");
}

#[tokio::test]
async fn checkstyle_rewritten_rename_keeps_the_predecessor() {
    let before = include_str!("fixtures/checkstyle-rename-before.java");
    let after = include_str!("fixtures/checkstyle-rename-after.java");
    let fixture = Fixture::new();
    let parent = fixture.commit(&[(CHECKER, before)], &[], "before listener refactor");
    let child = fixture.commit(&[(CHECKER, after)], &[&parent], "listener refactor");
    let selected = anchor(CHECKER, after, "protected void fireErrors(");
    let shown = fixture
        .history(&child, &format!("show {child} {selected}"))
        .await;
    assert!(
        shown.contains("-    private void displayErrors("),
        "{shown}"
    );
    assert!(shown.contains("+    protected void fireErrors("), "{shown}");
    assert!(!shown.contains("/dev/null"), "{shown}");
    whole_predecessor(
        &fixture,
        &child,
        &parent,
        &selected,
        before,
        "private void displayErrors(",
    )
    .await;
}

#[tokio::test]
async fn checkstyle_overload_keeps_the_implementation_and_wrapper_relationship() {
    let before = include_str!("fixtures/checkstyle-wrapper-before.java");
    let after = include_str!("fixtures/checkstyle-wrapper-after.java");
    let fixture = Fixture::new();
    let parent = fixture.commit(&[(COMMON, before)], &[], "before overload");
    let child = fixture.commit(&[(COMMON, after)], &[&parent], "extract overload");
    let selected = anchor(
        COMMON,
        after,
        "public static Pattern createPattern(String pattern, int flags)",
    );
    let shown = fixture
        .history(&child, &format!("show {child} {selected}"))
        .await;
    assert!(
        shown.contains("-    public static Pattern createPattern(String pattern)"),
        "{shown}"
    );
    let log = fixture
        .history(&child, &format!("log {selected} limit:1"))
        .await;
    assert!(log.contains("wrapper"), "{log}");
    whole_predecessor(
        &fixture,
        &child,
        &parent,
        &selected,
        before,
        "public static Pattern createPattern(String pattern)",
    )
    .await;
}

#[tokio::test]
async fn commons_lang_keeps_the_whole_renamed_implementation() {
    let before = include_str!("fixtures/lang-wrapper-before.java");
    let after = include_str!("fixtures/lang-wrapper-after.java");
    let fixture = Fixture::new();
    let parent = fixture.commit(&[(NUMBERS, before)], &[], "before deprecation");
    let child = fixture.commit(&[(NUMBERS, after)], &[&parent], "deprecate wrapper");
    let selected = anchor(NUMBERS, after, "public static boolean isCreatable(");
    let shown = fixture
        .history(&child, &format!("show {child} {selected}"))
        .await;
    assert!(
        shown.contains("-    public static boolean isNumber("),
        "{shown}"
    );
    assert!(
        shown.contains("+    public static boolean isCreatable("),
        "{shown}"
    );
    let log = fixture
        .history(&child, &format!("log {selected} limit:1"))
        .await;
    assert!(log.contains("wrapper"), "{log}");
    whole_predecessor(
        &fixture,
        &child,
        &parent,
        &selected,
        before,
        "public static boolean isNumber(",
    )
    .await;
}

#[tokio::test]
async fn junit_parent_two_import_is_not_a_merge_authored_edit() {
    let before1 = include_str!("fixtures/junit-before1.java");
    let before2 = include_str!("fixtures/junit-before2.java");
    let after = include_str!("fixtures/junit-after.java");
    let fixture = Fixture::new();
    let base = fixture.commit(&[(RUNNER, before1)], &[], "base");
    let left = fixture.commit(&[(RUNNER, before1)], &[&base], "first parent");
    let right = fixture.commit(&[(RUNNER, before2)], &[&base], "import implementation");
    let merge = fixture.commit(&[(RUNNER, after)], &[&left, &right], "merge");
    let selected = anchor(RUNNER, after, "protected void runChild(");
    let shown = fixture
        .history(&merge, &format!("show {merge} parent:2 {selected}"))
        .await;
    assert!(!shown.contains("diff --git"), "{shown}");
    let log = fixture
        .history(&merge, &format!("log {selected} limit:1"))
        .await;
    assert_eq!(ids(&log).first().copied(), Some(right.as_str()), "{log}");
    let first = fixture
        .history(&merge, &format!("show {merge} parent:1 {selected}"))
        .await;
    assert!(
        first.contains("-\tprotected void runChild(FrameworkMethod"),
        "{first}"
    );
    assert!(
        first.contains("+\tprotected void runChild(final FrameworkMethod"),
        "{first}"
    );
    let log = fixture
        .history(&merge, &format!("log {selected} limit:2"))
        .await;
    assert_eq!(ids(&log), [right.as_str(), base.as_str()], "{log}");
}

#[tokio::test]
async fn rust_lazy_cell_split_keeps_the_old_owner_and_path() {
    let before = include_str!("fixtures/rust-lazy-before.rs");
    let after = include_str!("fixtures/rust-lazy-after.rs");
    let stub = include_str!("fixtures/rust-lazy-stub.rs");
    let once = include_str!("fixtures/rust-once-after.rs");
    let fixture = Fixture::new();
    let parent = fixture.commit(&[("library/core/src/lazy.rs", before)], &[], "before split");
    let child = fixture.commit(
        &[
            ("library/core/src/lazy.rs", stub),
            ("library/core/src/cell/lazy.rs", after),
            ("library/core/src/cell/once.rs", once),
        ],
        &[&parent],
        "split lazy cells",
    );
    let shown = fixture
        .history(
            &child,
            &format!("show {child} method:force path:library/core/src/cell/lazy.rs"),
        )
        .await;
    assert!(shown.contains("a/library/core/src/lazy.rs"), "{shown}");
    assert!(shown.contains("b/library/core/src/cell/lazy.rs"), "{shown}");
    assert!(!shown.contains("/dev/null"), "{shown}");
}

const GUID: &str = "src/libraries/System.Private.CoreLib/src/System/Guid.cs";
const INTEGER: &str = "src/libraries/System.Private.CoreLib/src/System/Int32.cs";
const BUFFER: &str = "src/libraries/System.Private.CoreLib/src/System/Buffer.cs";
const BYTE_MEM_OPS: &str =
    "src/libraries/System.Private.CoreLib/src/System/SpanHelpers.ByteMemOps.cs";

fn source_pair(path: &str, before: &str, after: &str) -> (Fixture, String, String) {
    let fixture = Fixture::new();
    let parent = fixture.commit(&[(path, before)], &[], "original source");
    let child = fixture.commit(&[(path, after)], &[&parent], "changed source");
    (fixture, parent, child)
}

#[tokio::test]
async fn runtime_guid_generalization_keeps_the_written_predecessor() {
    let before = include_str!("fixtures/csharp-guid-utf8-before.cs");
    let after = include_str!("fixtures/csharp-guid-utf8-after.cs");
    let (fixture, parent, child) = source_pair(GUID, before, after);
    let selected = format!("method:*TryParseGuid* path:{GUID}");
    let shown = fixture
        .history(&child, &format!("show {child} {selected}"))
        .await;
    assert!(
        shown.contains("private static bool TryParseGuid(ReadOnlySpan<char>"),
        "{shown}"
    );
    assert!(
        shown.contains("private static bool TryParseGuid<TChar>"),
        "{shown}"
    );
    assert!(!shown.contains("/dev/null"), "{shown}");
    whole_predecessor(
        &fixture,
        &child,
        &parent,
        &selected,
        before,
        "private static bool TryParseGuid(",
    )
    .await;
}

#[tokio::test]
async fn runtime_guid_byte_overload_keeps_the_character_implementation_origin() {
    let (fixture, _, child) = source_pair(
        GUID,
        include_str!("fixtures/csharp-guid-utf8-before.cs"),
        include_str!("fixtures/csharp-guid-utf8-after.cs"),
    );
    let selected = format!("method:Guid.Parse(ReadOnlySpan<byte>) path:{GUID}");
    let shown = fixture
        .history(&child, &format!("show {child} {selected}"))
        .await;
    assert!(
        shown.contains("public static Guid Parse(ReadOnlySpan<char>"),
        "{shown}"
    );
    assert!(
        shown.contains("public static Guid Parse(ReadOnlySpan<byte>"),
        "{shown}"
    );
    assert!(!shown.contains("/dev/null"), "{shown}");
    let log = fixture
        .history(&child, &format!("log {selected} limit:1"))
        .await;
    assert!(log.contains("copy"), "{log}");
}

#[tokio::test]
async fn runtime_integer_span_parser_keeps_its_explicit_interface_origin() {
    let (fixture, _, child) = source_pair(
        INTEGER,
        include_str!("fixtures/csharp-int32-interface-before.cs"),
        include_str!("fixtures/csharp-int32-interface-after.cs"),
    );
    let selected =
        format!("method:Int32.Parse(ReadOnlySpan<char>,IFormatProvider?) path:{INTEGER}");
    let shown = fixture
        .history(&child, &format!("show {child} {selected}"))
        .await;
    assert!(
        shown.contains("-static int ISpanParseable<int>.Parse("),
        "{shown}"
    );
    assert!(
        shown.contains("+public static int Parse(ReadOnlySpan<char> s, IFormatProvider? provider)"),
        "{shown}"
    );
    assert!(!shown.contains("/dev/null"), "{shown}");
}

#[tokio::test]
async fn runtime_memmove_keeps_the_old_owner_and_path() {
    let fixture = Fixture::new();
    let parent = fixture.commit(
        &[(BUFFER, include_str!("fixtures/csharp-buffer-before.cs"))],
        &[],
        "before move",
    );
    let child = fixture.commit(
        &[
            (BUFFER, include_str!("fixtures/csharp-buffer-after.cs")),
            (
                BYTE_MEM_OPS,
                include_str!("fixtures/csharp-bytememops-after.cs"),
            ),
        ],
        &[&parent],
        "move byte operations",
    );
    let selected = format!("method:SpanHelpers.Memmove path:{BYTE_MEM_OPS}");
    let shown = fixture
        .history(&child, &format!("show {child} {selected}"))
        .await;
    assert!(shown.contains(&format!("a/{BUFFER}")), "{shown}");
    assert!(shown.contains(&format!("b/{BYTE_MEM_OPS}")), "{shown}");
    assert!(shown.contains("# move "), "{shown}");
    assert!(!shown.contains("/dev/null"), "{shown}");
}
