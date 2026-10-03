using System.Runtime.InteropServices;

// Runs alone in a read-only mount namespace, after project processes have exited.
static class PackageSnapshot
{
    [DllImport("libc", SetLastError = true)]
    static extern int link(string source, string destination);
    [DllImport("libc", SetLastError = true)]
    static extern long getxattr(string path, string name, IntPtr value, nuint size);

    static bool Redirected(string path) =>
        getxattr(path, "user.overlay.redirect", IntPtr.Zero, 0) >= 0 ||
        getxattr(path, "trusted.overlay.redirect", IntPtr.Zero, 0) >= 0;

    public static int Run(string lower, string upper, string destination)
    {
        Visit("");
        return 0;

        void Visit(string relative, bool changed = false)
        {
            Directory.CreateDirectory(Path.Combine(destination, relative));
            foreach (var source in Directory.EnumerateFileSystemEntries(Path.Combine("/tmp/package-view", relative)))
            {
                var name = Path.GetFileName(source);
                if (relative.Length == 0 && name == ".sigla-generation") continue;
                var path = Path.Combine(relative, name);
                var attributes = File.GetAttributes(source);
                if ((attributes & FileAttributes.ReparsePoint) != 0)
                    throw new IOException("Package snapshot contains a symbolic link");
                var written = Path.Combine(upper, path);
                if ((attributes & FileAttributes.Directory) != 0)
                    Visit(path, changed || Redirected(written));
                else
                {
                    var target = Path.Combine(destination, path);
                    var dirty = changed || Path.Exists(written) || !File.Exists(Path.Combine(lower, path));
                    if (dirty) File.Copy(source, target);
                    else if (link(Path.Combine(lower, path), target) != 0)
                        throw new IOException("Cannot link unchanged package file", Marshal.GetLastPInvokeError());
                }
            }
        }
    }
}
