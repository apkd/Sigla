# Unity native type catalogs

Native persistent IDs, native-to-managed identity, and managed class inheritance
are separate evidence. Do not derive IDs from assembly metadata, YAML root names,
C# source declarations, native inheritance, or namespace-prefix heuristics.

## Checked-in baseline and provenance

`2022.3.json` and `6000.0.json` are reviewed seed catalogs, not exports of Unity's
complete engine registry. They contain a coverage subset of the versioned public
ClassIDReference tables: 168 established managed mappings for 2022.3 and 169 for
6000.0, plus explicit unresolved entries. Their immediate managed bases were
reviewed against the corresponding UnityCsReference release branches. They were
not generated from locally executed Unity or locally inspected editor DLLs.

Native IDs and native names:

- https://docs.unity3d.com/2022.3/Documentation/Manual/ClassIDReference.html
- https://docs.unity3d.com/6000.0/Documentation/Manual/ClassIDReference.html

Managed identities and immediate class bases:

- https://github.com/Unity-Technologies/UnityCsReference/tree/2022.3
- https://github.com/Unity-Technologies/UnityCsReference/tree/6000.0

Relevant source families in each branch are `Runtime/Export/Scripting`,
`Runtime/Export/Transform`, `Runtime/Export/Graphics`, `Runtime/Export/Camera`,
`Runtime/Export/Shaders`, and the bindings in `Modules/Physics`, `Physics2D`,
`Audio`, `Animation`, `Grid`, `Tilemap`, `Director`, `VFX`, `AI`, `UI`, `Terrain`,
`TerrainPhysics`, `Vehicles`, `Wind`, `Cloth`, `ParticleSystem`, `SpriteMask`,
`SpriteShape`, `Video`, `TextRendering`, and `AssetBundle`. Editor animator types
are in `Editor/Mono/AnimatorController.bindings.cs`, `StateMachine.bindings.cs`,
`BlendTree.bindings.cs`, and `Editor/Mono/Animation`.

In particular, the catalogs retain these managed distinctions:

- AnimationClip -> Motion -> Object; AudioListener/AudioSource -> AudioBehaviour
  -> Behaviour; Collider2D -> Behaviour; ConstantForce2D -> PhysicsUpdateBehaviour2D.
- AnimatorController and animator graph subobjects use UnityEditor.Animations;
  PlayableDirector uses UnityEngine.Playables; constraints use
  UnityEngine.Animations; Tilemap types use UnityEngine.Tilemaps.
- ID 134 is PhysicMaterial in 2022.3 and PhysicsMaterial in 6000.0. The obsolete
  managed PhysicMaterial spelling in Unity 6 does not establish ID 134's identity.
- AudioClip derives directly from Object in 2022.3, but from
  UnityEngine.Audio.AudioResource in 6000.0.

`bindings.json` is the reviewed identity allowlist shared by the maintenance
pipeline. It deliberately contains neither numeric IDs nor base classes. A
matching short native name is only usable after its fully qualified managed
counterpart has been explicitly approved here. Class 114 is always excluded,
regardless of the allowlist: its identity comes from m_Script.

## Runtime policy

The saved project's ProjectVersion.txt selects an explicit major.minor catalog.
Only stable 2022.3 and 6000.0 branches are bundled. Other versions retain the
native identities shared by both catalogs, with an explicit incomplete-coverage
notice. Only agreed base links are retained; version-specific renames remain
unresolved. Matching editor metadata can supply the actual ancestry, but does
not establish version-specific native IDs. There is no nearest-version choice.
The branch snapshots are baselines, not a
claim that every patch release has an identical registry or hierarchy.

Matching metadata must already be published in Sigla's Store. It must belong to
this Unity root and carry the exact selected and declared editor version and
editor-reference provenance recorded by discovery. Source stubs, plugins, and
metadata for a substituted editor are excluded. DLL and .sigla inputs use the
same published declaration records; no assembly or editor is executed.

An observed class uses its matching metadata chain in full. Missing or ambiguous
bases produce only established partial ancestry and an explicit coverage reason;
no baseline bases are spliced into that chain. An absent class in its present
matching defining assembly does not fall back to a stale definition. If that
module's metadata is unavailable, the entire branch baseline chain is used.
Only observed native IDs and their necessary managed bases are resolved, once per
Unity project, and shared by composed objects.

Inheritance here means the managed superclass chain, not implemented interfaces.
Native-only, unapproved, or unlisted IDs remain unresolved. Public ClassIDReference
pages explicitly list common IDs, so even a complete scrape is not a complete
engine registry. Coverage reflects unresolved surviving objects in the selected
assets, including results beyond a query's display limit. Prefab removals do not
leave native-type coverage warnings on removed objects. Asset type labels do not
expand instance queries beyond Component/ScriptableObject ancestry.

## Updating a catalog offline

Use the official ClassIDReference HTML saved for the requested release branch,
plus DLLs from one matching editor installation. The maintenance exporter reads
PE metadata without loading target code or starting Unity:

```sh
cargo run --example unity_native_metadata -- \
  --version 6000.0.1f1 \
  --managed-dir /path/to/6000.0.1f1/Editor/Data/Managed \
  --output /tmp/unity-6000.0-managed.json

cargo run --example unity_native_catalog -- \
  --branch 6000.0 \
  --class-ids /path/to/saved/6000.0-ClassIDReference.html \
  --managed /tmp/unity-6000.0-managed.json \
  --output /tmp/6000.0.json
```

`--version` is an assertion about the installation that supplied the DLLs; CLR
assembly versions cannot independently establish an editor's Unity release.
Do not mix editor/player reference directories or multiple installations.
The exporter records relative assembly paths and content hashes. It rejects
duplicate assemblies and preserves ambiguous type identities as unresolved.

The generator verifies the documentation's version label independently of the
metadata version. It parses the numeric table (not the alphabetical duplicate),
retains unapproved entries as null, and rejects duplicate IDs, ambiguous approved
names, incomplete/cyclic managed chains, and approved non-UnityEngine.Object
classes. A changed documentation format fails validation rather than silently
assigning a different release's IDs. Output records source hashes for both
identity and ancestry inputs. It does not fetch anything from the network.

Review the generated diff before replacing the baseline. Approve additional
native-to-managed bindings only with independent identity evidence. For a new
release branch, add the snapshot to native.rs and extend the version/behavior
fixtures; never silently broaden an existing snapshot's version range. Nothing
in the maintenance pipeline runs during compilation or asset search.

```sh
cargo test --example unity_native_catalog
cargo test --lib unity::assets::native::tests
cargo test --test integration unity_assets
```
