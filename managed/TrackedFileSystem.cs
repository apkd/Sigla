using Microsoft.Build.FileSystem;

// Evaluation sees the complete tracked tree, but reading omitted contents requests
// materialization. All source membership and conditions remain MSBuild's work.
sealed class TrackedFileSystem : MSBuildFileSystemBase
{
    readonly HashSet<string> files;
    readonly HashSet<string> directories = new(StringComparer.Ordinal);
    readonly HashSet<string> required;
    public TrackedFileSystem(HashSet<string> files, HashSet<string> required)
    {
        this.files = files;
        this.required = required;
        foreach (var file in files)
            for (var directory = Path.GetDirectoryName(file); directory != null; directory = Path.GetDirectoryName(directory))
                directories.Add(directory);
    }
    static string Full(string path) => Path.TrimEndingDirectorySeparator(Path.GetFullPath(path));
    void Reading(string path)
    {
        path = Full(path);
        if (files.Contains(path) && !File.Exists(path))
        {
            required.Add(path);
            throw new IOException("An omitted tracked input must be materialized");
        }
    }
    public override TextReader ReadFile(string path) { Reading(path); return base.ReadFile(path); }
    public override Stream GetFileStream(string path, FileMode mode, FileAccess access, FileShare share)
    { Reading(path); return base.GetFileStream(path, mode, access, share); }
    public override string ReadFileAllText(string path) { Reading(path); return base.ReadFileAllText(path); }
    public override byte[] ReadFileAllBytes(string path) { Reading(path); return base.ReadFileAllBytes(path); }
    public override bool FileExists(string path) => files.Contains(Full(path)) || base.FileExists(path);
    public override bool DirectoryExists(string path) => directories.Contains(Full(path)) || base.DirectoryExists(path);
    public override bool FileOrDirectoryExists(string path) => FileExists(path) || DirectoryExists(path);
    public override FileAttributes GetAttributes(string path) => base.FileOrDirectoryExists(path)
        ? base.GetAttributes(path) : directories.Contains(Full(path)) ? FileAttributes.Directory : FileAttributes.Normal;
    public override DateTime GetLastWriteTimeUtc(string path) => base.FileOrDirectoryExists(path)
        ? base.GetLastWriteTimeUtc(path) : DateTime.UnixEpoch;

    IEnumerable<string> Virtual(IEnumerable<string> entries, string path, string pattern, SearchOption option)
    {
        path = Full(path);
        foreach (var entry in entries)
            if ((option == SearchOption.TopDirectoryOnly ? Path.GetDirectoryName(entry) == path : entry.StartsWith(path + Path.DirectorySeparatorChar, StringComparison.Ordinal))
                && System.IO.Enumeration.FileSystemName.MatchesSimpleExpression(pattern, Path.GetFileName(entry), false))
                yield return entry;
    }
    public override IEnumerable<string> EnumerateFiles(string path, string pattern, SearchOption option) =>
        (base.DirectoryExists(path) ? base.EnumerateFiles(path, pattern, option) : [])
        .Concat(Virtual(files, path, pattern, option)).Distinct(StringComparer.Ordinal);
    public override IEnumerable<string> EnumerateDirectories(string path, string pattern, SearchOption option) =>
        (base.DirectoryExists(path) ? base.EnumerateDirectories(path, pattern, option) : [])
        .Concat(Virtual(directories, path, pattern, option)).Distinct(StringComparer.Ordinal);
    public override IEnumerable<string> EnumerateFileSystemEntries(string path, string pattern, SearchOption option) =>
        EnumerateFiles(path, pattern, option).Concat(EnumerateDirectories(path, pattern, option));
}
