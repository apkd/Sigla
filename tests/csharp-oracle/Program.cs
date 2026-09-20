using System.Text.Json;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.CSharp.Syntax;

// Offline reference only. Input files mark navigation sites with /*bind:name*/.
var trees = args.Select(path => CSharpSyntaxTree.ParseText(
    File.ReadAllText(path), new CSharpParseOptions(LanguageVersion.CSharp14), path)).ToArray();
var references = ((string)AppContext.GetData("TRUSTED_PLATFORM_ASSEMBLIES")!).Split(Path.PathSeparator)
    .Select(path => MetadataReference.CreateFromFile(path));
var compilation = CSharpCompilation.Create("Fixture", trees, references,
    new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary));
var results = new List<object>();
foreach (var tree in trees)
{
    var model = compilation.GetSemanticModel(tree);
    var root = tree.GetRoot();
    foreach (var marker in root.DescendantTrivia().Where(t => t.ToString().StartsWith("/*bind:")))
    {
        var label = marker.ToString()[7..^2];
        var token = root.FindToken(marker.Span.End);
        if (token.SpanStart < marker.Span.End) token = token.GetNextToken();
        SyntaxNode node = token.Parent!;
        while (node.Parent is MemberAccessExpressionSyntax member && member.Name == node
            || node.Parent is InvocationExpressionSyntax call && call.Expression == node)
            node = node.Parent!;
        var info = model.GetSymbolInfo(node);
        var type = model.GetTypeInfo(node);
        results.Add(new {
            label,
            file = tree.FilePath,
            start = token.SpanStart,
            symbol = Identity(info.Symbol),
            candidates = info.CandidateSymbols.Select(Identity),
            reason = info.CandidateReason.ToString(),
            type = type.Type?.ToDisplayString(SymbolDisplayFormat.FullyQualifiedFormat),
            convertedType = type.ConvertedType?.ToDisplayString(SymbolDisplayFormat.FullyQualifiedFormat)
        });
    }
}
Console.WriteLine(JsonSerializer.Serialize(new {
    compiler = typeof(CSharpCompilation).Assembly.GetName().Version?.ToString(),
    references = ((string)AppContext.GetData("TRUSTED_PLATFORM_ASSEMBLIES")!).Split(Path.PathSeparator)
        .Where(path => new[] { "System.Private.CoreLib.dll", "System.Runtime.dll", "System.Linq.dll", "System.Collections.dll", "netstandard.dll" }.Contains(Path.GetFileName(path))),
    results,
    errors = compilation.GetDiagnostics().Where(d => d.Severity == DiagnosticSeverity.Error)
        .Select(d => d.ToString())
}, new JsonSerializerOptions { WriteIndented = true }));

static object? Identity(ISymbol? symbol) => symbol is null ? null : new {
    definition = symbol.OriginalDefinition.GetDocumentationCommentId(),
    assembly = symbol.ContainingAssembly?.Identity.ToString(),
    containingType = symbol.ContainingType?.ToDisplayString(SymbolDisplayFormat.FullyQualifiedFormat),
    typeArguments = symbol is IMethodSymbol method
        ? method.TypeArguments.Select(t => t.ToDisplayString(SymbolDisplayFormat.FullyQualifiedFormat)).ToArray()
        : [],
    display = symbol.ToDisplayString(SymbolDisplayFormat.FullyQualifiedFormat)
};
