using System;
using System.Runtime.CompilerServices;
[assembly: TypeForwardedTo(typeof(System.Collections.Generic.List<>))]
public class GlobalType { public int Value; public static T Make<T>() => default!; }
namespace MetadataFixture
{
    public interface IParser { string Parse(string input); }
    public class Generic<T> where T : class
    {
        public T? Field;
        public T Echo(T value) => value;
        public U Identity<U>(U value) => value;
        public U Create<U>() where U : new() => new();
        public int[,] Matrix(int[,] value) => value;
        public int[][] Jagged(int[][] value) => value;
        public ref int Ref(ref int value) => ref value;
        public unsafe delegate*<int, int> Callback(delegate*<int, int> f) => f;
        public unsafe delegate* unmanaged[Cdecl]<int, int> NativeCallback(delegate* unmanaged[Cdecl]<int, int> f) => f;
        public Nested<U> NestedValue<U>(Nested<U> value) => value;
        public T? Property { get; set; }
        public event Action? Changed;
        public void Raise() => Changed?.Invoke();
        public class Nested<U> { public U? Value; }
    }
    public class Parser : IParser
    {
        string IParser.Parse(string input) => input;
        public virtual string Parse(int input) => input.ToString();
    }
    public class Derived : Parser { public override string Parse(int input) => base.Parse(input); }
}
