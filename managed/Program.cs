using System.Runtime.CompilerServices;
using System.Text.Json;
using Microsoft.Build.Locator;
using System.Runtime.InteropServices;

// Register before the CLR loads any method using Microsoft.Build types.
if (args[0] == "--resolve-sdk") return Sdk.Resolve(args[1], args[2], args[3], args[4]);
// Request structured import events, including conditional and missing imports.
Environment.SetEnvironmentVariable("MSBUILDLOGIMPORTS", "1");
MSBuildLocator.RegisterMSBuildPath(args[0]);
return Discovery.Run(args[1], args[2]);

static class Sdk
{
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    delegate void Result(int key, [MarshalAs(UnmanagedType.LPUTF8Str)] string value);
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    delegate int ResolveSdk([MarshalAs(UnmanagedType.LPUTF8Str)] string host, [MarshalAs(UnmanagedType.LPUTF8Str)] string directory, int flags, Result result);
    public static int Resolve(string host, string directory, string library, string output)
    {
        var handle = NativeLibrary.Load(library);
        try
        {
            var resolve = Marshal.GetDelegateForFunctionPointer<ResolveSdk>(NativeLibrary.GetExport(handle, "hostfxr_resolve_sdk2"));
            string? sdk = null;
            Result callback = (key, value) => { if (key == 0) sdk = value; };
            var status = resolve(host, directory, 0, callback);
            GC.KeepAlive(callback);
            if (status != 0 || sdk == null) throw new InvalidOperationException("No installed SDK satisfies global.json");
            File.WriteAllText(output, JsonSerializer.Serialize(new { Sdk = sdk }));
            return 0;
        }
        finally { NativeLibrary.Free(handle); }
    }
}

static class Discovery
{
    static readonly string[] Targets = [
        "PrepareForBuild", "AddImplicitDefineConstants", "FindReferenceAssembliesForReferences",
        "GenerateGlobalUsings", "GenerateAssemblyInfo", "GenerateTargetFrameworkMonikerAttribute"
    ];

    [MethodImpl(MethodImplOptions.NoInlining)]
    public static int Run(string requestFile, string resultFile)
    {
        try
        {
            var request = JsonSerializer.Deserialize<Request>(File.ReadAllText(requestFile))!;
            using var collection = new Microsoft.Build.Evaluation.ProjectCollection();
            var required = new HashSet<string>(StringComparer.Ordinal);
            var tracked = new HashSet<string>(request.Tracked ?? [], StringComparer.Ordinal);
            void Require(string path) { if (tracked.Contains(path) && !File.Exists(path)) required.Add(path); }
            int Missing()
            {
                File.WriteAllText(resultFile, JsonSerializer.Serialize(new { Version = 1, NeedsRestore = false,
                    DependencyState = new Dictionary<string,string>(), Projects = Array.Empty<object>(), RequiredInputs = required.Order().ToArray() }));
                return 0;
            }
            var defaults = new Dictionary<string, string> {
                ["DesignTimeBuild"] = "true", ["BuildProjectReferences"] = "false",
                ["SkipCompilerExecution"] = "true", ["ProvideCommandLineArgs"] = "true"
            };
            if (request.ArtifactsPath is { } artifacts) {
                defaults["UseArtifactsOutput"] = "true";
                defaults["IncludeProjectNameInArtifactsPaths"] = "true";
                defaults["ArtifactsPath"] = artifacts;
            }
            var entries = new List<Microsoft.Build.Graph.ProjectGraphEntryPoint>();
            void Probe(string entry, IDictionary<string, string> globals)
            {
                if (tracked.Count == 0) return;
                Require(entry);
                if (required.Count != 0) return;
                var imports = new Imports();
                using var probeCollection = new Microsoft.Build.Evaluation.ProjectCollection();
                probeCollection.RegisterLogger(imports);
                try
                {
                    var probe = Microsoft.Build.Evaluation.Project.FromFile(entry, new Microsoft.Build.Definition.ProjectOptions {
                        GlobalProperties = globals, ProjectCollection = probeCollection,
                        LoadSettings = Microsoft.Build.Evaluation.ProjectLoadSettings.IgnoreMissingImports,
                        EvaluationContext = Microsoft.Build.Evaluation.Context.EvaluationContext.Create(
                            Microsoft.Build.Evaluation.Context.EvaluationContext.SharingPolicy.Shared,
                            new TrackedFileSystem(tracked, required))
                    });
                    foreach (var kind in new[] { "Compile", "ProjectReference" })
                        foreach (var item in probe.GetItems(kind)) Require(Path.GetFullPath(item.EvaluatedInclude, probe.DirectoryPath));
                    foreach (var item in probe.GetItems("Reference")) {
                        var hint = item.GetMetadataValue("HintPath");
                        if (hint.Length != 0) Require(Path.GetFullPath(hint, probe.DirectoryPath));
                    }
                }
                catch when (required.Count != 0) { }
                foreach (var import in imports.Events)
                    if (!string.IsNullOrEmpty(import.ImportedProjectFile)) Require(import.ImportedProjectFile);
            }
            void AddEntry(string entry, Dictionary<string, string> globals)
            {
                Probe(entry, globals);
                if (required.Count != 0) return;
                var project = new Microsoft.Build.Evaluation.Project(entry, globals, null, collection);
                var properties = new Dictionary<string, string>(globals);
                var framework = project.GetPropertyValue("TargetFramework");
                if (framework.Length == 0)
                    framework = project.GetPropertyValue("TargetFrameworks").Split(';', StringSplitOptions.RemoveEmptyEntries).FirstOrDefault() ?? "";
                if (framework.Length != 0) properties["TargetFramework"] = framework;
                entries.Add(new(entry, properties));
                collection.UnloadProject(project);
            }
            var diagnostics = new List<string>();
            void TryAddEntry(string path, Dictionary<string, string> properties) {
                try { AddEntry(path, properties); }
                catch (Exception e) when (required.Count == 0) { diagnostics.Add($"Excluded project {path}: {e.Message}"); }
            }
            foreach (var entry in request.Entries)
            {
                try {
                if (Path.GetExtension(entry) is ".sln" or ".slnx")
                {
                    var solution = Microsoft.Build.Construction.SolutionFile.Parse(entry);
                    var configuration = solution.GetDefaultConfigurationName() + "|" + solution.GetDefaultPlatformName();
                    foreach (var project in solution.ProjectsInOrder.Where(p => p.AbsolutePath.EndsWith(".csproj", StringComparison.OrdinalIgnoreCase)))
                    {
                        var properties = new Dictionary<string, string>(defaults);
                        if (project.ProjectConfigurations.TryGetValue(configuration, out var mapping))
                        {
                            if (!mapping.IncludeInBuild) continue;
                            properties["Configuration"] = mapping.ConfigurationName;
                            properties["Platform"] = mapping.PlatformName;
                        }
                        TryAddEntry(project.AbsolutePath, properties);
                    }
                    continue;
                }
                TryAddEntry(entry, defaults);
                } catch (Exception e) when (required.Count == 0) { diagnostics.Add($"Excluded entry {entry}: {e.Message}"); }
            }
            if (required.Count != 0) return Missing();
            Microsoft.Build.Graph.ProjectGraph CreateGraph(IEnumerable<Microsoft.Build.Graph.ProjectGraphEntryPoint> roots) =>
                new(roots, collection, (path, properties, projects) => {
                    Probe(path, properties);
                    if (required.Count != 0) throw new IOException("Tracked inputs require materialization");
                    var project = new Microsoft.Build.Evaluation.Project(path, properties, null, projects);
                    var instance = project.CreateProjectInstance();
                    projects.UnloadProject(project);
                    return instance;
                }, 1, CancellationToken.None);
            var nodes = new List<Microsoft.Build.Graph.ProjectGraphNode>();
            try { nodes.AddRange(CreateGraph(entries).ProjectNodesTopologicallySorted); }
            catch when (required.Count != 0) { return Missing(); }
            catch (Exception e) {
                diagnostics.Add($"Combined project graph is incomplete: {e.Message}. Evaluating entries separately.");
                foreach (var entry in entries) {
                    try { nodes.AddRange(CreateGraph([entry]).ProjectNodesTopologicallySorted); }
                    catch when (required.Count != 0) { return Missing(); }
                    catch (Exception error) { diagnostics.Add($"Excluded project graph {entry.ProjectFile}: {error.Message}"); }
                }
            }
            nodes = nodes.DistinctBy(node => Identity(node.ProjectInstance)).ToList();
            if (required.Count != 0) return Missing();
            var dependencyState = new Dictionary<string, string>();
            var outputOwners = new Dictionary<string, string>(StringComparer.Ordinal);
            var restore = false;
            foreach (var node in nodes)
            {
                var project = node.ProjectInstance;
                var assets = project.GetPropertyValue("ProjectAssetsFile");
                if (assets.Length == 0 || project.GetPropertyValue("IsCrossTargetingBuild") == "true") continue;
                if (request.ArtifactsPath != null) {
                    var output = Path.GetFullPath(assets, project.Directory);
                    if (outputOwners.TryGetValue(output, out var owner) && owner != project.FullPath)
                        throw new InvalidOperationException($"Projects share dependency output {output}: {owner} and {project.FullPath}");
                    outputOwners[output] = project.FullPath;
                }
                var identity = Identity(project);
                var fingerprint = JsonSerializer.Serialize(new {
                    Properties = project.Properties.Where(p => p.Name.StartsWith("Restore", StringComparison.OrdinalIgnoreCase)
                        || p.Name is "TargetFramework" or "TargetFrameworks" or "RuntimeIdentifier" or "RuntimeIdentifiers" or "NuGetPackageRoot")
                        .OrderBy(p => p.Name).Select(p => new { p.Name, p.EvaluatedValue }).ToArray(),
                    Items = new[] { "PackageReference", "PackageVersion", "ProjectReference", "FrameworkReference", "PackageDownload" }
                        .SelectMany(name => project.GetItems(name).Select(i => new { Kind = name, i.EvaluatedInclude,
                            Metadata = i.Metadata.OrderBy(m => m.Name).Select(m => new { m.Name, m.EvaluatedValue }).ToArray() })).ToArray()
                });
                dependencyState[identity] = fingerprint;
                bool usable = false;
                if (File.Exists(assets))
                {
                    try {
                    using var parsed = JsonDocument.Parse(File.ReadAllText(assets));
                    usable = parsed.RootElement.TryGetProperty("targets", out var targets)
                        && targets.TryGetProperty(project.GetPropertyValue("TargetFramework"), out _);
                    } catch (Exception e) when (e is IOException or JsonException) {
                        diagnostics.Add($"Unavailable dependency assets {assets}: {e.Message}");
                    }
                }
                if (!usable || !request.Restored && request.DependencyState.TryGetValue(identity, out var previous) && previous != fingerprint)
                    restore = true;
            }
            if (restore)
            {
                if (request.Restored) diagnostics.Add("Restore did not produce all dependency assets. Using available project details.");
                else {
                File.WriteAllText(resultFile, JsonSerializer.Serialize(new { Version = 1, NeedsRestore = true, DependencyState = dependencyState, Projects = Array.Empty<object>() }));
                return 0;
                }
            }
            var errors = new List<string>();
            using var manager = new Microsoft.Build.Execution.BuildManager();
            manager.BeginBuild(new Microsoft.Build.Execution.BuildParameters(collection) {
                EnableNodeReuse = false, MaxNodeCount = 1,
                Loggers = [new Errors(errors)]
            });
            var projects = new List<object>();
            try
            {
                foreach (var node in nodes)
                {
                    var initial = node.ProjectInstance;
                    try {
                    errors.Clear();
                    if (initial.GetPropertyValue("IsCrossTargetingBuild") == "true") continue;
                    var targets = Targets.Where(initial.Targets.ContainsKey).ToArray();
                    var final = initial;
                    if (targets.Length != 0)
                    {
                        var result = manager.BuildRequest(new Microsoft.Build.Execution.BuildRequestData(
                            initial, targets, null, Microsoft.Build.Execution.BuildRequestDataFlags.ProvideProjectStateAfterBuild));
                        if (result.OverallResult != Microsoft.Build.Execution.BuildResultCode.Success)
                            diagnostics.Add($"Incomplete build details for {initial.FullPath}: {string.Join("; ", errors)}");
                        final = result.ProjectStateAfterBuild ?? initial;
                    }
                    var evaluated = new Microsoft.Build.Evaluation.Project(initial.FullPath,
                        initial.GlobalProperties.ToDictionary(p => p.Key, p => p.Value), null, collection);
                    string Full(string p) => Path.GetFullPath(p, evaluated.DirectoryPath);
                    string[] Items(string name) => final.GetItems(name).Select(i => Full(i.EvaluatedInclude)).Distinct().ToArray();
                    var properties = new[] { "AssemblyName", "DefineConstants", "LangVersion", "Nullable",
                        "AllowUnsafeBlocks", "CheckForOverflowUnderflow", "TargetFramework", "Configuration",
                        "ProjectAssetsFile", "MSBuildProjectExtensionsPath", "IntermediateOutputPath", "TargetPath", "TargetRefPath" }
                        .ToDictionary(p => p, final.GetPropertyValue);
                    projects.Add(new {
                        Identity = Identity(initial), Origin = initial.FullPath,
                        Properties = properties, Sources = Items("Compile"),
                        References = node.ProjectReferences.Where(r => final.GetItems("ProjectReference")
                            .Any(i => Full(i.EvaluatedInclude) == r.ProjectInstance.FullPath && !i.GetMetadataValue("ReferenceOutputAssembly").Equals("false", StringComparison.OrdinalIgnoreCase)))
                            .Select(r => new { Identity = Identity(r.ProjectInstance),
                                Aliases = final.GetItems("ProjectReference").First(i => Full(i.EvaluatedInclude) == r.ProjectInstance.FullPath).GetMetadataValue("Aliases") }).ToArray(),
                        Assemblies = (targets.Length == 0 ? final.GetItems("Reference") : final.GetItems("ReferencePathWithRefAssemblies")).Select(i => new {
                            Path = Full(targets.Length == 0 ? i.GetMetadataValue("HintPath") : i.EvaluatedInclude), Aliases = i.GetMetadataValue("Aliases"),
                            SourceProject = i.GetMetadataValue("MSBuildSourceProjectFile")
                        }).ToArray(),
                        Imports = evaluated.Imports.Select(i => i.ImportedProject.FullPath).Distinct().ToArray(),
                        Globs = evaluated.GetAllGlobs("Compile").SelectMany(g => g.IncludeGlobs)
                            .Select(g => Microsoft.Build.Globbing.MSBuildGlob.Parse(evaluated.DirectoryPath, g).FixedDirectoryPart).Distinct().ToArray()
                    });
                    collection.UnloadProject(evaluated);
                    } catch (Exception e) { diagnostics.Add($"Excluded project {initial.FullPath}: {e.Message}"); }
                }
            }
            finally { manager.EndBuild(); }
            File.WriteAllText(resultFile, JsonSerializer.Serialize(new { Version = 1, NeedsRestore = false, DependencyState = dependencyState, Projects = projects, Diagnostics = diagnostics }));
            return 0;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine(e.Message);
            return 1;
        }
    }

    static string Identity(Microsoft.Build.Execution.ProjectInstance project)
    {
        var properties = project.GlobalProperties.ToDictionary(p => p.Key, p => p.Value);
        foreach (var name in new[] { "Configuration", "Platform", "TargetFramework" })
            properties[name] = project.GetPropertyValue(name);
        return JsonSerializer.Serialize(new { Path = project.FullPath, Properties = properties.OrderBy(p => p.Key).ToArray() });
    }

    sealed record Request(string[] Entries, Dictionary<string, string> DependencyState, bool Restored, string[]? Tracked, string? ArtifactsPath);
    sealed class Imports : Microsoft.Build.Framework.ILogger
    {
        public List<Microsoft.Build.Framework.ProjectImportedEventArgs> Events { get; } = [];
        public Microsoft.Build.Framework.LoggerVerbosity Verbosity { get; set; } = Microsoft.Build.Framework.LoggerVerbosity.Diagnostic;
        public string? Parameters { get; set; }
        public void Initialize(Microsoft.Build.Framework.IEventSource source) => source.AnyEventRaised += (_, e) => {
            if (e is Microsoft.Build.Framework.ProjectImportedEventArgs import) Events.Add(import);
        };
        public void Shutdown() { }
    }
    sealed class Errors(List<string> errors) : Microsoft.Build.Framework.ILogger
    {
        public Microsoft.Build.Framework.LoggerVerbosity Verbosity { get; set; }
        public string? Parameters { get; set; }
        public void Initialize(Microsoft.Build.Framework.IEventSource source) =>
            source.ErrorRaised += (_, e) => errors.Add($"{e.Code}: {e.Message}");
        public void Shutdown() { }
    }
}
