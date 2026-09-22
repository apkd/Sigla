using System;
using System.IO;
using System.Linq;
using UnityEditor;
using UnityEditor.Compilation;
using UnityEngine;

public static class ExportCompilation
{
    [Serializable] public sealed class CaptureRequest { public string directory; public int stage; }
    static double readyAfter;
    static bool removingBridge;

    [InitializeOnLoadMethod]
    static void StartCapturePolling()
    {
        readyAfter = EditorApplication.timeSinceStartup + 3;
        EditorApplication.update += CapturePending;
    }

    static void CapturePending()
    {
        if (EditorApplication.isCompiling || EditorApplication.isUpdating) {
            readyAfter = EditorApplication.timeSinceStartup + 3;
            return;
        }
        if (EditorApplication.timeSinceStartup < readyAfter) return;
        var root = Directory.GetParent(Application.dataPath).FullName;
        var requestPath = Path.Combine(root, "capture-request.json");
        if (!File.Exists(requestPath)) return;
        if (File.ReadAllText(Path.Combine(root, "Packages/manifest.json")).Contains("dev.tryfinally.conduit") ||
            UnityEditor.PackageManager.PackageInfo.GetAllRegisteredPackages().Any(p => p.name == "dev.tryfinally.conduit")) {
            // The bridge's editor assembly requires Unity 6. The 2022 capture fixture
            // disables that assembly and removes the bootstrap package before export.
            if (!removingBridge) {
                removingBridge = true;
                UnityEditor.PackageManager.Client.Remove("dev.tryfinally.conduit");
            }
            return;
        }
        var request = JsonUtility.FromJson<CaptureRequest>(File.ReadAllText(requestPath));
        Directory.CreateDirectory(request.directory);
        if (request.stage == 0) {
            request.stage = 1;
            File.WriteAllText(requestPath, JsonUtility.ToJson(request));
            EditorUserBuildSettings.development = false;
            PlayerSettings.SetApiCompatibilityLevel(UnityEditor.Build.NamedBuildTarget.Standalone, (ApiCompatibilityLevel)6);
            readyAfter = EditorApplication.timeSinceStartup + 3;
            return;
        }
        var profile = request.stage == 1 ? "standard" : "framework";
        Capture(Path.Combine(request.directory, "editor-" + profile + ".json"), false);
        Capture(Path.Combine(request.directory, "player-" + profile + ".json"), true);
        if (request.stage == 1) {
            request.stage = 2;
            File.WriteAllText(requestPath, JsonUtility.ToJson(request));
            PlayerSettings.SetApiCompatibilityLevel(UnityEditor.Build.NamedBuildTarget.Standalone, (ApiCompatibilityLevel)3);
            readyAfter = EditorApplication.timeSinceStartup + 3;
        } else File.Delete(requestPath);
    }

    [Serializable] public sealed class AssemblyData
    {
        public string name;
        public string[] sources;
        public string[] defines;
        public string[] projects;
        public string[] references;
        public string[] responseFiles;
        public string flags;
    }

    [Serializable] public sealed class InputData
    {
        public string path;
        public string contents;
        public string base64;
    }

    [Serializable] public sealed class Snapshot
    {
        public string version;
        public string revision;
        public string target;
        public string api;
        public int apiValue;
        public bool development;
        public string context;
        public AssemblyData[] assemblies;
        public InputData[] inputs;
    }

    public static void Capture(string destination, bool player)
    {
        AssetDatabase.SaveAssets();
        var root = Directory.GetParent(Application.dataPath).FullName;
        var packages = UnityEditor.PackageManager.PackageInfo.GetAllRegisteredPackages();
        string Normalize(string path)
        {
            path = path.Replace('\\', '/');
            var editor = EditorApplication.applicationContentsPath.Replace('\\', '/');
            if (path.StartsWith(editor + "/", StringComparison.Ordinal))
                return "<EDITOR_DATA>/" + path.Substring(editor.Length + 1);
            foreach (var package in packages.OrderByDescending(p => p.resolvedPath.Length))
                if (path.StartsWith(package.resolvedPath + "/", StringComparison.Ordinal))
                    return "Packages/" + package.name + "/" + path.Substring(package.resolvedPath.Length + 1);
            if (path.StartsWith(root + "/", StringComparison.Ordinal))
                return path.Substring(root.Length + 1);
            if (Path.IsPathRooted(path))
                throw new InvalidOperationException("Unmapped compilation path: " + path);
            return path;
        }

        if (EditorUserBuildSettings.activeBuildTarget != BuildTarget.StandaloneLinux64)
            throw new InvalidOperationException("Select Linux standalone before exporting");
        if (EditorUserBuildSettings.development)
            throw new InvalidOperationException("Disable development builds before exporting");
        var api = PlayerSettings.GetApiCompatibilityLevel(UnityEditor.Build.NamedBuildTarget.Standalone);
        var assemblies = CompilationPipeline.GetAssemblies(player ? AssembliesType.PlayerWithoutTestAssemblies : AssembliesType.Editor);
        var inputPaths = new[] {Path.Combine(root, "Assets"), Path.Combine(root, "Packages"), Path.Combine(root, "FixturePackages")}
            .Where(Directory.Exists).SelectMany(p => Directory.GetFiles(p, "*", SearchOption.AllDirectories))
            .Where(p => new[] { ".cs", ".asmdef", ".asmref", ".meta", ".rsp", ".dll" }.Contains(Path.GetExtension(p)) || Path.GetFileName(p) == "package.json")
            .Concat(new[] { "ProjectSettings/ProjectVersion.txt", "ProjectSettings/ProjectSettings.asset",
                "Packages/manifest.json", "Packages/packages-lock.json" }.Select(p => Path.Combine(root, p)));
        var snapshot = new Snapshot {
            version = Application.unityVersion,
            revision = UnityEditorInternal.InternalEditorUtility.GetFullUnityVersion(),
            target = EditorUserBuildSettings.activeBuildTarget.ToString(),
            api = api.ToString(), apiValue = (int)api,
            development = EditorUserBuildSettings.development,
            context = player ? "UNITY_STANDALONE_LINUX" : "UNITY_EDITOR_LINUX",
            assemblies = assemblies.OrderBy(a => a.name).Select(a => new AssemblyData {
                name = a.name, flags = a.flags.ToString(),
                sources = a.sourceFiles.Select(Normalize).OrderBy(p => p).ToArray(),
                defines = a.defines.Concat(a.compilerOptions.ResponseFiles.SelectMany(p =>
                    CompilationPipeline.ParseResponseFile(p, root, a.compiledAssemblyReferences.Select(Path.GetDirectoryName).Distinct().ToArray()).Defines))
                    .Distinct().OrderBy(d => d).ToArray(),
                projects = a.assemblyReferences.Select(r => r.name).OrderBy(p => p).ToArray(),
                references = a.compiledAssemblyReferences.Concat(a.compilerOptions.ResponseFiles.SelectMany(p =>
                    CompilationPipeline.ParseResponseFile(p, root, a.compiledAssemblyReferences.Select(Path.GetDirectoryName).Distinct().ToArray()).FullPathReferences))
                    .Select(Normalize).Distinct().OrderBy(p => p).ToArray(),
                responseFiles = a.compilerOptions.ResponseFiles.Select(Normalize).OrderBy(p => p).ToArray()
            }).ToArray(),
            inputs = inputPaths.OrderBy(p => p).Select(p => new InputData {
                path = p.Substring(root.Length + 1).Replace('\\', '/'),
                base64 = Path.GetExtension(p) == ".dll" ? Convert.ToBase64String(File.ReadAllBytes(p)) : null,
                contents = Path.GetExtension(p) == ".dll" ? null : File.ReadAllText(p).Replace(root, "<PROJECT>")
                    .Replace("/home/apk/src/conduit/Conduit.Unity", "<TOOLS>/Conduit.Unity")
            }).ToArray()
        };
        File.WriteAllText(destination, JsonUtility.ToJson(snapshot, true));
    }
}
