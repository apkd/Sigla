use super::*;
use crate::model::position;

fn extract_surface(source: &str) -> Facts {
    super::extract(source, Language::ShaderLab).unwrap()
}

fn names(facts: &Facts, kind: &str) -> Vec<String> {
    facts
        .declarations
        .iter()
        .filter(|declaration| declaration.kind == kind)
        .map(|declaration| declaration.name.clone())
        .collect()
}

fn entries(facts: &Facts) -> Vec<String> {
    facts
        .occurrences
        .iter()
        .zip(&facts.native.as_ref().unwrap().occurrences)
        .filter(|(_, info)| info.role == Role::EntryPoint)
        .map(|(occurrence, _)| occurrence.name.clone())
        .collect()
}

fn invariant(source: &str, facts: &Facts) {
    let native = facts.native.as_ref().unwrap();
    assert_eq!(facts.declarations.len(), native.declarations.len());
    assert_eq!(facts.occurrences.len(), native.occurrences.len());
    let mut expected_names = std::collections::BTreeMap::<String, Vec<u32>>::new();
    let mut expected_calls = std::collections::BTreeMap::<u32, Vec<u32>>::new();
    let check = |range: &Range<usize>| {
        assert!(
            range.start <= range.end && range.end <= source.len(),
            "{range:?}"
        );
        assert!(source.is_char_boundary(range.start) && source.is_char_boundary(range.end));
    };
    for (declaration, info) in facts.declarations.iter().zip(&native.declarations) {
        check(&declaration.name_span);
        check(&declaration.span);
        check(&declaration.header);
        check(&declaration.scope);
        assert!(
            native.regions[info.region as usize]
                .span
                .contains(&declaration.name_span.start)
        );
        if let Some(parent) = info.parent {
            assert!((parent as usize) < facts.declarations.len());
        }
    }
    for (index, (occurrence, info)) in facts
        .occurrences
        .iter()
        .zip(&native.occurrences)
        .enumerate()
    {
        check(&occurrence.span);
        if let Some(receiver) = &info.receiver {
            check(receiver);
        }
        if let Some(assignment) = &info.assignment {
            check(assignment);
        }
        assert!(
            native.regions[info.region as usize]
                .span
                .contains(&occurrence.span.start)
        );
        if let Some(local) = info.local {
            assert!(facts.declarations[local as usize].local());
        }
        if let Some(owner) = info.owner {
            assert!((owner as usize) < facts.declarations.len());
            if occurrence.call {
                expected_calls.entry(owner).or_default().push(index as u32);
            }
        }
        expected_names
            .entry(occurrence.name.clone())
            .or_default()
            .push(index as u32);
    }
    assert!(facts.occurrences.windows(2).all(|pair| {
        (pair[0].span.start, pair[0].span.end) <= (pair[1].span.start, pair[1].span.end)
    }));
    assert_eq!(native.names, expected_names);
    assert_eq!(native.calls, expected_calls);
    for include in &native.includes {
        check(&include.span);
    }
}

#[test]
fn extension_mapping_and_shader_group() {
    for (extension, expected) in [
        ("ngcompute", Language::Hlsl),
        ("ngshader", Language::Hlsl),
        ("ngsurface", Language::ShaderLab),
        ("ngsurfshader", Language::ShaderLab),
        ("surfshader", Language::ShaderLab),
        ("shader", Language::ShaderLab),
        ("compute", Language::Hlsl),
    ] {
        let path = format!("Assets/Test.{extension}");
        assert_eq!(
            crate::native::language(std::path::Path::new(&path)),
            Some(expected)
        );
        assert_eq!(
            crate::native::Group::of(expected),
            Some(crate::native::Group::Shaders)
        );
    }
    // The native map's existing case-sensitive policy is not changed globally.
    assert_eq!(
        crate::native::language(std::path::Path::new("test.NGSHADER")),
        None
    );
}

#[test]
fn unmodified_languages_keep_identical_facts() {
    for (language, source) in [
        (
            Language::Hlsl,
            "#pragma kernel K\n[numthreads(1,1,1)] void K(uint3 id : SV_DispatchThreadID) {}",
        ),
        (
            Language::ShaderLab,
            "Shader \"X\" { HLSLPROGRAM\n#pragma fragment Frag\nfloat4 Frag() : SV_Target { return 1; }\nENDHLSL }",
        ),
        (Language::Glsl, "void main() {}"),
        (Language::C, "int f(int x) { return x + 1; }"),
        (
            Language::Cpp,
            "struct S { int value; }; int f() { S s; return s.value; }",
        ),
    ] {
        let baseline = super::super::extract::extract(source, language).unwrap();
        let adapted = super::extract(source, language).unwrap();
        assert_eq!(format!("{baseline:?}"), format!("{adapted:?}"));
        invariant(source, &adapted);
    }
}

#[test]
fn properties_do_not_become_hlsl_functions() {
    let source = "BEGIN_PROPERTIES\n[MainColor] _Tint(\"Tint\", Color) = (1, .5, 0, 1)\n_Gain(\"Gain\", Range(0, 2)) = 1\nEND_PROPERTIES\n#pragma native_pass Draw _ Vert Frag\nfloat4 Vert() : SV_Position { return _Tint; }\nfloat4 Frag() : SV_Target { return _Gain; }\n";
    let facts = super::extract(source, Language::Hlsl).unwrap();
    assert_eq!(names(&facts, "property"), ["_Tint", "_Gain"]);
    assert_eq!(names(&facts, "function"), ["Vert", "Frag"]);
    assert_eq!(entries(&facts), ["Vert", "Frag"]);
    let tint = facts
        .declarations
        .iter()
        .find(|declaration| declaration.name == "_Tint")
        .unwrap();
    assert_eq!(tint.ty, "Color");
    assert_eq!(tint.attributes, ["[MainColor]"]);
    assert_eq!(&source[tint.name_span.clone()], "_Tint");
    invariant(source, &facts);
}

#[test]
fn all_corpus_property_kinds_and_multiline_attributes() {
    let source = r#"BEGIN_PROPERTIES
[Header("Section with ] and )")]
[Enum(One, 1, Two, 2)] _Float("Float", Float) = -1e-3
_Int("Integer", Integer) = 2
_Range("Range", Range(-1, 4)) = .5
_Color("Color", Color) =
    (1, 0, 0, 1)
_Vector("Vector", Vector) = (0, 1, 2, 3)
_Texture("Texture", 2D) = "white" {}
_Array("Array", 2DArray) = "" {}
END_PROPERTIES
"#;
    let prepared = prepare(source, Language::Hlsl);
    assert!(!prepared.errors);
    assert_eq!(prepared.declarations.len(), 7);
    let float = &prepared.declarations[0];
    assert_eq!(float.attributes.len(), 2);
    assert_eq!(&source[float.name.clone()], "_Float");
    assert_eq!(float.ty, "Float");
}

#[test]
fn surface_code_defines_and_legacy_cbuffer_share_one_region() {
    let source = "BEGIN_OPTIONS\nShaderName \"Example\"\nEND_OPTIONS\nBEGIN_CBUFFER(Material)\nfloat _Scale;\nEND_CBUFFER\nBEGIN_DEFINES\n#define SCALE _Scale\nEND_DEFINES\nBEGIN_CODE\n#include \"Leaf.hlsl\"\nfloat Shared(float x) { return x * SCALE; }\nEND_CODE\nBEGIN_CODE\nfloat SurfaceFunction(float x) { return Shared(x); }\nEND_CODE\n";
    let facts = extract_surface(source);
    assert_eq!(names(&facts, "function"), ["Shared", "SurfaceFunction"]);
    assert_eq!(names(&facts, "macro"), ["SCALE"]);
    assert_eq!(facts.native.as_ref().unwrap().regions.len(), 1);
    assert_eq!(facts.native.as_ref().unwrap().includes[0].path, "Leaf.hlsl");
    assert!(
        facts
            .declarations
            .iter()
            .any(|declaration| declaration.name == "_Scale")
    );
    assert!(
        !facts
            .declarations
            .iter()
            .any(|declaration| declaration.name == "ShaderName")
    );
    invariant(source, &facts);
}

#[test]
fn varying_fields_are_not_semantic_selectors() {
    let source = "BEGIN_VARYINGS\nGBuffer Normal TexCoord0\nGBuffer KEY : TangentBasis\n* HK_CUTOUT : TexCoord0\n* : float4 lineData linear\nGBuffer KEY : uint material nointerpolation\nEND_VARYINGS\nBEGIN_CODE\nvoid ModifyVertex(inout VertexData v) { v.lineData = 1; }\nfloat4 SurfaceFunction(ShaderData d) { return d.lineData; }\nEND_CODE\n";
    let facts = extract_surface(source);
    assert_eq!(names(&facts, "field"), ["lineData", "material"]);
    let index = facts
        .declarations
        .iter()
        .position(|declaration| declaration.name == "lineData")
        .unwrap();
    assert_eq!(facts.declarations[index].ty, "float4");
    assert!(!facts.native.as_ref().unwrap().declarations[index].conditional);
    let material = facts
        .declarations
        .iter()
        .position(|declaration| declaration.name == "material")
        .unwrap();
    assert!(facts.native.as_ref().unwrap().declarations[material].conditional);
    assert!(
        facts.occurrences.iter().any(
            |occurrence| occurrence.name == "lineData" && occurrence.write == WriteKind::Direct
        )
    );
    assert!(
        facts
            .occurrences
            .iter()
            .any(|occurrence| occurrence.name == "lineData" && occurrence.write == WriteKind::None)
    );
    invariant(source, &facts);
}

#[test]
fn unicode_crlf_bom_and_original_columns_are_preserved() {
    let source = "\u{feff}begin_properties\r\n[Header(\"Żółć\")] _Tint(\"Café\", Color) = (1,1,1,1)\r\nend_properties\r\nbegin_code\r\n// 日本語\r\nfloat Read() { return _Tint.x; }\r\nend_code\r\n";
    let prepared = prepare(source, Language::ShaderLab);
    assert_eq!(prepared.text.len(), source.len());
    for (index, byte) in source.bytes().enumerate() {
        assert_eq!(
            matches!(byte, b'\r' | b'\n'),
            matches!(prepared.text.as_bytes()[index], b'\r' | b'\n')
        );
        if matches!(byte, b'\r' | b'\n') {
            assert_eq!(byte, prepared.text.as_bytes()[index]);
        }
    }
    assert!(!prepared.errors);
    let facts = extract_surface(source);
    let tint = facts
        .declarations
        .iter()
        .find(|declaration| declaration.name == "_Tint")
        .unwrap();
    assert_eq!(position(source, tint.name_span.start), (2, 18));
    let read = facts
        .declarations
        .iter()
        .find(|declaration| declaration.name == "Read")
        .unwrap();
    assert_eq!(position(source, read.name_span.start), (6, 7));
    invariant(source, &facts);
}

#[test]
fn commented_or_quoted_markers_do_not_change_modes() {
    let source = "/*\nBEGIN_PROPERTIES\n_Bad(\"X\", Float) = 1\nEND_PROPERTIES\n*/\n// BEGIN_CODE\nfloat f() { return 1; }\n";
    let prepared = prepare(source, Language::Hlsl);
    assert_eq!(prepared.text, source);
    assert!(prepared.declarations.is_empty());
    let source =
        "BEGIN_CODE\n/*\nEND_CODE\nBEGIN_PROPERTIES\n*/\nfloat f() { return 1; }\nEND_CODE\n";
    let facts = extract_surface(source);
    assert_eq!(names(&facts, "function"), ["f"]);
    invariant(source, &facts);
    let source = "Shader \"BEGIN_CODE\" { HLSLPROGRAM\nfloat f() { return 1; }\nENDHLSL }";
    assert_eq!(
        prepare(source, Language::ShaderLab).language,
        Language::ShaderLab
    );
}

#[test]
fn native_entries_geometry_mesh_repetition_and_continuations() {
    let source = "#pragma native_pass First Tag Vert Frag Geom\n#pragma native_mesh First Mesh KEY\n#pragma native_pass Second _ Vert \\\r\n Frag\n#pragma native_channels First Position\n#pragma native_define First VALUE=1\n#pragma native_material_contract Material\n";
    let (spans, errors) = entry_points(source, 0..source.len());
    assert!(!errors);
    assert_eq!(
        spans
            .iter()
            .map(|span| &source[span.clone()])
            .collect::<Vec<_>>(),
        ["Vert", "Frag", "Geom", "Mesh", "Vert", "Frag"]
    );
    let facts = super::extract(source, Language::Hlsl).unwrap();
    assert_eq!(
        entries(&facts),
        ["Vert", "Frag", "Geom", "Mesh", "Vert", "Frag"]
    );
    assert!(facts.occurrences.iter().all(|occurrence| !occurrence.call));
    invariant(source, &facts);
}

#[test]
fn native_directives_in_comments_literals_and_macro_bodies_are_ignored() {
    let source = "// #pragma native_pass Bad _ X Y\n/*\n#pragma native_mesh Bad Z KEY\n*/\nconst char* s = \"#pragma native_pass Bad _ X Y\";\n#define FAKE \\\n#pragma native_pass Bad _ X Y\n/* prefix comment */ #pragma native_pass Good _ Vert Frag\n";
    let (spans, errors) = entry_points(source, 0..source.len());
    assert!(!errors);
    assert_eq!(
        spans
            .iter()
            .map(|span| &source[span.clone()])
            .collect::<Vec<_>>(),
        ["Vert", "Frag"]
    );
}

#[test]
fn malformed_directives_do_not_invent_entry_points() {
    let source = "#pragma native_pass P _ V\n#pragma native_mesh P M\n#pragma native_pass P _ V F Extra TooMany\n#pragma native_pass Good _ V F\n";
    let (spans, errors) = entry_points(source, 0..source.len());
    assert!(errors);
    assert_eq!(
        spans
            .iter()
            .map(|span| &source[span.clone()])
            .collect::<Vec<_>>(),
        ["V", "F"]
    );
}

#[test]
fn unterminated_and_mismatched_blocks_recover_later_code() {
    for source in [
        "BEGIN_CODE\nfloat Retained() { return 1; }\n",
        "END_OPTIONS\nBEGIN_CODE\nfloat Retained() { return 1; }\nEND_CODE\n",
        "BEGIN_OPTIONS\nName \"missing close\"\nBEGIN_CODE\nfloat Retained() { return 1; }\nEND_CODE\n",
        "BEGIN_CODE\nfloat Retained() { return 1; }\nEND_DEFINES\n",
    ] {
        let facts = extract_surface(source);
        assert!(facts.errors);
        assert_eq!(names(&facts, "function"), ["Retained"]);
        invariant(source, &facts);
    }
}

#[test]
fn unsupported_blocks_and_bad_metadata_are_marked_incomplete() {
    let source = "BEGIN_BLACKBOARD\nunknown stuff\nEND_BLACKBOARD\nBEGIN_PROPERTIES\nBad(\"Bad\", Unsupported) = 0\nGood(\"Good\", Float) = 1\nEND_PROPERTIES\nBEGIN_VARYINGS\n* : float4\nEND_VARYINGS\nBEGIN_CODE\nfloat Retained() { return Good; }\nEND_CODE\n";
    let facts = extract_surface(source);
    assert!(facts.errors);
    assert_eq!(names(&facts, "function"), ["Retained"]);
    assert_eq!(names(&facts, "property"), ["Good"]);
    invariant(source, &facts);
}

#[test]
fn added_entries_preserve_call_owners_local_shadowing_and_writes() {
    let source = "BEGIN_PROPERTIES\n_Value(\"Value\", Float) = 1\nEND_PROPERTIES\n#pragma native_pass P _ Vert Frag\nfloat Helper(float x) { return x; }\nfloat4 Vert() : SV_Position { float _Value = 2; _Value += 1; return Helper(_Value); }\nfloat4 Frag() : SV_Target { return _Value; }\n";
    let facts = super::extract(source, Language::Hlsl).unwrap();
    let native = facts.native.as_ref().unwrap();
    let vert = facts
        .declarations
        .iter()
        .position(|declaration| declaration.name == "Vert")
        .unwrap();
    assert!(
        native.calls[&(vert as u32)]
            .iter()
            .any(|&index| facts.occurrences[index as usize].name == "Helper")
    );
    assert!(
        facts
            .occurrences
            .iter()
            .zip(&native.occurrences)
            .any(|(occurrence, info)| {
                occurrence.name == "_Value"
                    && info.local.is_some()
                    && occurrence.write == WriteKind::Direct
            })
    );
    let property = facts
        .declarations
        .iter()
        .position(|declaration| declaration.kind == "property")
        .unwrap();
    assert!(
        native
            .occurrences
            .iter()
            .all(|info| info.local != Some(property as u32))
    );
    invariant(source, &facts);
}

#[test]
fn property_text_cannot_create_pragmas_or_calls() {
    let source = "BEGIN_PROPERTIES\n_Tint(\"#pragma native_pass Fake _ X Y\", Color) = (1,1,1,1)\n// #pragma native_mesh Fake Mesh KEY\nEND_PROPERTIES\n#pragma native_pass Real _ Vert Frag\n";
    let facts = super::extract(source, Language::Hlsl).unwrap();
    assert_eq!(entries(&facts), ["Vert", "Frag"]);
    assert!(
        !facts
            .occurrences
            .iter()
            .any(|occurrence| occurrence.name == "Fake" || occurrence.name == "Mesh")
    );
    invariant(source, &facts);
}

#[test]
fn existing_shaderlab_regions_do_not_merge() {
    let source = "Shader \"X\" { Pass { HLSLPROGRAM\n#pragma native_pass P _ V F\nfloat V() { return 1; } float F() { return 1; }\nENDHLSL } Pass { HLSLPROGRAM\nfloat V() { return 2; }\nENDHLSL } }";
    let facts = extract_surface(source);
    assert_eq!(facts.native.as_ref().unwrap().regions.len(), 2);
    assert_eq!(
        names(&facts, "function")
            .iter()
            .filter(|name| name.as_str() == "V")
            .count(),
        2
    );
    assert_eq!(entries(&facts), ["V", "F"]);
    invariant(source, &facts);
}

#[test]
fn comments_touching_property_delimiters_keep_their_boundaries() {
    let source = "/* leading comment\n*/ BEGIN_PROPERTIES\n_P(\"P\", Float) = 1\nEND_PROPERTIES /* trailing comment\ncontinues here */\nfloat Retained() { return _P; }\n";
    let prepared = prepare(source, Language::Hlsl);
    assert!(!prepared.errors);
    assert!(prepared.text.starts_with("/* leading comment\n*/ "));
    assert!(
        prepared
            .text
            .contains("/* trailing comment\ncontinues here */")
    );
    let facts = super::extract(source, Language::Hlsl).unwrap();
    assert_eq!(names(&facts, "property"), ["_P"]);
    assert_eq!(names(&facts, "function"), ["Retained"]);
    invariant(source, &facts);
}

#[test]
fn invalid_marker_tail_does_not_report_clean_empty_facts() {
    let source = "BEGIN_CODE invalid\nfloat f() {}\n";
    assert!(prepare(source, Language::ShaderLab).errors);
}

#[test]
fn public_dispatch_and_serialized_facts_use_the_adapter() {
    let source = "BEGIN_PROPERTIES\n_P(\"P\", Float) = 1\nEND_PROPERTIES\nBEGIN_CODE\nfloat Read() { return _P; }\nEND_CODE\n";
    let language = crate::native::language(std::path::Path::new("Test.ngsurface")).unwrap();
    let facts = crate::extract::extract(source, language, &[], "").unwrap();
    assert_eq!(names(&facts, "property"), ["_P"]);
    assert_eq!(names(&facts, "function"), ["Read"]);
    let bytes = postcard::to_allocvec(&facts).unwrap();
    let restored: Facts = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(format!("{facts:?}"), format!("{restored:?}"));
    invariant(source, &restored);
}

#[test]
fn raw_hlsl_marker_like_macro_invocations_are_not_containers() {
    for source in [
        "BEGIN_PROPERTIES();\nfloat Value;\n",
        "BEGIN_PROPERTIES()\nfloat Value;\n",
    ] {
        let prepared = prepare(source, Language::Hlsl);
        assert_eq!(prepared.text, source);
        assert!(!prepared.errors);
        assert!(prepared.declarations.is_empty());
    }
}
#[test]
fn code_openers_preserve_trailing_multiline_comments() {
    for opening in ["BEGIN_CODE", "BEGIN_DEFINES", "BEGIN_CBUFFER(Material)"] {
        let closing = opening.split('(').next().unwrap().replace("BEGIN_", "END_");
        let source = format!(
            "{opening} /* comment starts here\nfloat Hidden;\n*/\nfloat Visible;\n{closing}\n"
        );
        let facts = extract_surface(&source);
        assert!(!facts.errors);
        assert!(facts.declarations.iter().any(|d| d.name == "Visible"));
        assert!(!facts.declarations.iter().any(|d| d.name == "Hidden"));
        invariant(&source, &facts);
    }
}
