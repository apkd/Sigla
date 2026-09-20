#nullable enable
using System.Reflection.Metadata;
using System.Reflection.PortableExecutable;
using System.Text.Json;
using System.Collections.Immutable;

// This test oracle reads metadata only. It never loads target assemblies.
foreach (var path in args.Where(a => a != "--signatures"))
{
    using var stream = File.OpenRead(path);
    using var pe = new PEReader(stream);
    var metadata = pe.GetMetadataReader();
    var types = metadata.TypeDefinitions.Select(h => metadata.GetTypeDefinition(h)).ToArray();
    if (args.Contains("--signatures"))
    {
        var provider = new SignatureNames();
        var signatures = new List<object>();
        foreach (var handle in metadata.TypeDefinitions)
        {
            var type = metadata.GetTypeDefinition(handle);
            if (metadata.GetString(type.Name) == "<Module>") continue;
            var owner = provider.GetTypeFromDefinition(metadata, handle, 0);
            foreach (var methodHandle in type.GetMethods())
            {
                var method = metadata.GetMethodDefinition(methodHandle);
                var signature = method.DecodeSignature(provider, (object?)null);
                var name = metadata.GetString(method.Name);
                signatures.Add(new { qualified = owner + "." + name, kind = name is ".ctor" or ".cctor" ? "constructor" : "method", ty = signature.ReturnType, parameters = signature.ParameterTypes });
            }
            foreach (var fieldHandle in type.GetFields())
            {
                var field = metadata.GetFieldDefinition(fieldHandle);
                signatures.Add(new { qualified = owner + "." + metadata.GetString(field.Name), kind = "field", ty = field.DecodeSignature(provider, (object?)null), parameters = Array.Empty<string>() });
            }
            foreach (var propertyHandle in type.GetProperties())
            {
                var property = metadata.GetPropertyDefinition(propertyHandle);
                var signature = property.DecodeSignature(provider, (object?)null);
                signatures.Add(new { qualified = owner + "." + metadata.GetString(property.Name), kind = "property", ty = signature.ReturnType, parameters = signature.ParameterTypes });
            }
        }
        Console.WriteLine(JsonSerializer.Serialize(signatures));
        continue;
    }
    Console.WriteLine(JsonSerializer.Serialize(new
    {
        path,
        types = types.Count(t => metadata.GetString(t.Name) != "<Module>"),
        methods = metadata.MethodDefinitions.Count,
        fields = metadata.FieldDefinitions.Count,
        properties = metadata.PropertyDefinitions.Count,
        events = metadata.EventDefinitions.Count,
        interfaces = types.Sum(t => t.GetInterfaceImplementations().Count),
        nested = types.Count(t => t.IsNested),
        genericParameters = types.Sum(t => t.GetGenericParameters().Count)
            + metadata.MethodDefinitions.Sum(h => metadata.GetMethodDefinition(h).GetGenericParameters().Count),
        methodImplementations = types.Sum(t => t.GetMethodImplementations().Count),
        forwarders = metadata.ExportedTypes.Count(h => metadata.GetExportedType(h).IsForwarder),
        globalTypes = types.Where(t => metadata.GetString(t.Namespace) == "" && !t.IsNested)
            .Select(t => metadata.GetString(t.Name)).Where(n => n != "<Module>").ToArray()
    }));
}

sealed class SignatureNames : ISignatureTypeProvider<string, object?>
{
    static string Qualify(string ns, string name) => ns.Length == 0 ? name : ns + "." + name;
    public string GetTypeFromDefinition(MetadataReader reader, TypeDefinitionHandle handle, byte rawTypeKind)
    {
        var type = reader.GetTypeDefinition(handle);
        var name = reader.GetString(type.Name);
        return type.IsNested ? GetTypeFromDefinition(reader, type.GetDeclaringType(), 0) + "." + name : Qualify(reader.GetString(type.Namespace), name);
    }
    public string GetTypeFromReference(MetadataReader reader, TypeReferenceHandle handle, byte rawTypeKind)
    {
        var type = reader.GetTypeReference(handle);
        var name = reader.GetString(type.Name);
        return type.ResolutionScope.Kind == HandleKind.TypeReference ? GetTypeFromReference(reader, (TypeReferenceHandle)type.ResolutionScope, 0) + "." + name : Qualify(reader.GetString(type.Namespace), name);
    }
    public string GetTypeFromSpecification(MetadataReader reader, object? context, TypeSpecificationHandle handle, byte rawTypeKind) => reader.GetTypeSpecification(handle).DecodeSignature(this, context);
    public string GetPrimitiveType(PrimitiveTypeCode type) => type switch
    {
        PrimitiveTypeCode.Void => "void", PrimitiveTypeCode.Boolean => "bool", PrimitiveTypeCode.Char => "char",
        PrimitiveTypeCode.SByte => "sbyte", PrimitiveTypeCode.Byte => "byte", PrimitiveTypeCode.Int16 => "short", PrimitiveTypeCode.UInt16 => "ushort",
        PrimitiveTypeCode.Int32 => "int", PrimitiveTypeCode.UInt32 => "uint", PrimitiveTypeCode.Int64 => "long", PrimitiveTypeCode.UInt64 => "ulong",
        PrimitiveTypeCode.Single => "float", PrimitiveTypeCode.Double => "double", PrimitiveTypeCode.String => "string", PrimitiveTypeCode.Object => "object",
        PrimitiveTypeCode.IntPtr => "nint", PrimitiveTypeCode.UIntPtr => "nuint", PrimitiveTypeCode.TypedReference => "System.TypedReference", _ => throw new BadImageFormatException()
    };
    public string GetArrayType(string element, ArrayShape shape) => element + "[" + (shape.Rank == 1 ? "*" : new string(',', shape.Rank - 1)) + "]";
    public string GetSZArrayType(string element) => element + "[]";
    public string GetByReferenceType(string element) => "ref " + element;
    public string GetPointerType(string element) => element + "*";
    public string GetGenericInstantiation(string type, ImmutableArray<string> args) => type + "<" + string.Join(",", args) + ">";
    public string GetGenericMethodParameter(object? context, int index) => "!!" + index;
    public string GetGenericTypeParameter(object? context, int index) => "!" + index;
    public string GetModifiedType(string modifier, string element, bool required) => element;
    public string GetPinnedType(string element) => element;
    public string GetFunctionPointerType(MethodSignature<string> signature)
    {
        var convention = signature.Header.CallingConvention switch
        {
            SignatureCallingConvention.CDecl => " unmanaged[Cdecl]", SignatureCallingConvention.StdCall => " unmanaged[Stdcall]",
            SignatureCallingConvention.ThisCall => " unmanaged[Thiscall]", SignatureCallingConvention.FastCall => " unmanaged[Fastcall]",
            SignatureCallingConvention.Unmanaged => " unmanaged", SignatureCallingConvention.VarArgs => " vararg", _ => ""
        };
        return "delegate*" + convention + "<" + string.Join(",", signature.ParameterTypes.Append(signature.ReturnType)) + ">";
    }
}
