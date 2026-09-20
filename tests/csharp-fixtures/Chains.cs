using System;
using System.Collections.Generic;
using System.Linq;

namespace Navigation;

public class Singleton<T> where T : new()
{
    public static T Instance { get; } = new();
}
public class Pool
{
    public void Initialize() { }
}
public interface IComponent { void Register(); }
public class GameObject { }
public static class Components
{
    public static IEnumerable<T> Enumerate<T>(this GameObject gameObject, bool inactive) => [];
}
public class Enemy
{
    public bool IsAlive => true;
    public Pool Transform => new();
}
public class Base { public virtual void Run() { } }
public class Derived : Base { public override void Run() { } }
public interface IBag<out T> { }
public class Bag<T> : IBag<T> { }
public class Plain { public void Finish() { } }
public class Lifted { public void Finish() { } }
public class Counted { public int Count => 0; }
public static class Tasks
{
    static T Merge<T>(T first, T second) => first;
    static T First<T>(IBag<T> bag) => default!;
    static T Pack<T>(params T[] items) => default!;
    static Plain Pick(int count) => new();
    static Lifted Pick(int? count) => new();
    static R Apply<T, R>(T value, Func<T, R> convert) => convert(value);
    static Pool Convert(Enemy enemy) => new();
    public static void Execute(GameObject gameObject, IEnumerable<Enemy> enemies)
    {
        Singleton<Pool>.Instance./*bind:singleton*/Initialize();
        foreach (var item in gameObject.Enumerate<IComponent>(true))
            item./*bind:foreach*/Register();
        var selected = enemies.Where(enemy => enemy./*bind:lambda-input*/IsAlive)
                              .Select(enemy => enemy./*bind:lambda-output*/Transform);
        Base value = new Derived();
        value./*bind:static-receiver*/Run();
        var length = "text"./*bind:primitive-member*/Length;
    }
    public static void Inference(Derived derived, Base basis, Bag<Pool> bag, Pool[] pools, Counted counted, Enemy enemy)
    {
        Merge(derived, basis)./*bind:lower-bound*/Run();
        First(bag)./*bind:interface-projection*/Initialize();
        Pack(pools)./*bind:params-array*/Initialize();
        Pack(new Pool(), new Pool())./*bind:params-expanded*/Initialize();
        Pick(counted?.Count)./*bind:nullable-overload*/Finish();
        Apply(enemy, Convert)./*bind:method-group*/Initialize();
        foreach (var selected in pools.Select(pool => pool))
            selected./*bind:array-linq*/Initialize();
    }
}
