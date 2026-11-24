using System.Net;
using System.Net.Sockets;
using MwbBridgeCompanion;

var failures = 0;
void Check(bool ok, string what)
{
    Console.WriteLine($"{(ok ? "ok  " : "FAIL")} {what}");
    if (!ok) failures++;
}

const string Key = "SharedKey0123456";
var listener = new TcpListener(IPAddress.Loopback, 0);
listener.Start();
var port = ((IPEndPoint)listener.LocalEndpoint).Port;
var serverTask = Task.Run(() => FrameConnection.Open(listener.AcceptTcpClient(), Key));
using var client = FrameConnection.Open(new TcpClient("127.0.0.1", port), Key);
using var server = await serverTask;

// Empty frames (Active, FileEnd, Done) once crashed the decryptor.
client.WriteFrame(Frames.Active, ReadOnlySpan<byte>.Empty);
client.WriteJson(Frames.Inactive, new InactiveMsg(true));
client.WriteJson(Frames.Hello, new HelloMsg("control", "mac", Version: Frames.Version));
var (t1, b1) = server.ReadFrame();
Check(t1 == Frames.Active && b1.Length == 0, "an empty frame arrives empty");
Check(FrameConnection.OwnSalt(Key).Salt.SequenceEqual(FrameConnection.OwnSalt(Key).Salt) && !FrameConnection.OwnSalt(Key).Salt.SequenceEqual(FrameConnection.OwnSalt(Key + "x").Salt), "this side keeps one salt per security key");
var someSalt = new byte[16];
Check(FrameConnection.PeerKey(Key, someSalt).SequenceEqual(FrameConnection.Derive(Key, someSalt)) && FrameConnection.PeerKey(Key, someSalt).SequenceEqual(FrameConnection.Derive(Key, someSalt)), "a cached peer key matches a fresh derivation");
Check(server.ReadJson<InactiveMsg>(Frames.Inactive).DragOver, "JSON uses the bridge's snake_case names");
Check(server.ReadJson<HelloMsg>(Frames.Hello).Version == Frames.Version, "the hello carries the protocol version");
Check(FrameConnection.Parse<HelloMsg>(System.Text.Encoding.UTF8.GetBytes("{\"role\":\"control\",\"name\":\"mac\"}")).Version == 0, "a hello without a version is version 0");

// Two files with the same name in one offer mustn't overwrite each other.
Check(Names.Unique(new[] { "a.txt", "A.txt", "a.txt", "b" }).SequenceEqual(new[] { "a.txt", "A (2).txt", "a (3).txt", "b" }), "same-named files get unique names");
Check(Names.Unique(new[] { "x:y?.txt", "..", "" }).SequenceEqual(new[] { "x_y_.txt", "file", "file (2)" }), "unsafe names are cleaned up, then made unique");
Check(Names.Unique(new[] { "notes.txt", "notes.txt. " }).SequenceEqual(new[] { "notes.txt", "notes (2).txt" }), "trailing dots don't make a name different");

// An older bridge sends button_held; a newer one drag_over (and button_held
// for older companions).
InactiveMsg Inactive(string json) => FrameConnection.Parse<InactiveMsg>(System.Text.Encoding.UTF8.GetBytes(json));
var v1 = Inactive("{\"button_held\":true}");
// Let go on the Mac, then pressed again for something else: still ended.
var dropped = Inactive("{\"drag_over\":false,\"button_held\":true,\"let_go\":true}");
Check(v1.DragCameBack(0) && !dropped.DragCameBack(2), "INACTIVE is read the way the bridge's version means it");
Check(Inactive("{\"drag_over\":false,\"button_held\":false,\"let_go\":false}").DragCameBack(2), "a quick bounce that beat the offer came back");
Check(Inactive("{\"drag_over\":true,\"button_held\":true,\"let_go\":false}").DragCameBack(2), "a drag still over the Mac came back");

var dir = Path.Combine(Path.GetTempPath(), $"companion-tests-{Environment.ProcessId}");
Directory.CreateDirectory(dir);
var big = Enumerable.Range(0, Frames.ChunkSize + 1000).Select(i => (byte)(i % 251)).ToArray();
File.WriteAllBytes(Path.Combine(dir, "big.bin"), big);
File.WriteAllBytes(Path.Combine(dir, "empty.txt"), Array.Empty<byte>());
var sent = Task.Run(() => client.SendFiles(new[] { (Path.Combine(dir, "big.bin"), "big.bin"), (Path.Combine(dir, "gone.txt"), "gone.txt"), (Path.Combine(dir, "empty.txt"), "empty.txt") }));
var done = new List<int>();
server.ReceiveFiles(h => Path.Combine(dir, "got-" + h.Name), done.Add);
await sent;
Check(File.Exists(Path.Combine(dir, "got-gone.txt")) && new FileInfo(Path.Combine(dir, "got-gone.txt")).Length == 0, "a file gone since the offer arrives empty, keeping the others' places");
Check(File.ReadAllBytes(Path.Combine(dir, "got-big.bin")).SequenceEqual(big), "a file bigger than a chunk arrives intact");
Check(File.Exists(Path.Combine(dir, "got-empty.txt")) && new FileInfo(Path.Combine(dir, "got-empty.txt")).Length == 0, "an empty file arrives");
Check(done.SequenceEqual(new[] { 0, 1, 2 }), "each file is reported done, in order");

// Folders: offered as their files with relative names, rebuilt on arrival.
Directory.CreateDirectory(Path.Combine(dir, "photos", "2026"));
Directory.CreateDirectory(Path.Combine(dir, "photos", "empty"));
File.WriteAllBytes(Path.Combine(dir, "photos", "b.jpg"), new byte[] { 1 });
File.WriteAllBytes(Path.Combine(dir, "photos", "2026", "a.jpg"), new byte[] { 2 });
var skippedItems = new List<string>();
var expanded = Names.Expand(new[] { Path.Combine(dir, "photos"), Path.Combine(dir, "big.bin"), Path.Combine(dir, "missing") }, skippedItems.Add);
Check(expanded.Select(e => e.Name).SequenceEqual(new[] { "photos/b.jpg", "photos/2026/a.jpg", "big.bin" }), "a folder is offered as its files, by path from the folder");
Check(expanded[0].Size == 1 && expanded[2].Size == (ulong)big.Length, "sizes come with the files");
Check(skippedItems.SequenceEqual(new[] { Path.Combine(dir, "missing") }), "what can't be read is reported, not offered");
var (laidOut, topLevel) = Names.Layout(new[] { "photos/2026/a.jpg", "photos/b.jpg", "photos", "../evil.txt", "photos/2026/a.jpg", "x/y:z.txt" });
var sep = Path.DirectorySeparatorChar;
Check(laidOut.SequenceEqual(new[] { $"photos{sep}2026{sep}a.jpg", $"photos{sep}b.jpg", "photos (2)", "evil.txt", $"photos{sep}2026{sep}a (2).jpg", $"x{sep}y_z.txt" }), "folders are rebuilt safely, without climbing out");
Check(topLevel.SequenceEqual(new[] { "photos", "photos (2)", "evil.txt", "x" }), "only top-level items are handed on");
var (collided, collidedTop) = Names.Layout(new[] { "docs/readme.", "docs/readme/notes.txt", "Photos/a.txt", "photos/a.txt", "CON/x.txt", "lpt1.log" });
Check(collided.SequenceEqual(new[] { $"docs{sep}readme", $"docs{sep}readme (2){sep}notes.txt", $"Photos{sep}a.txt", $"photos (2){sep}a.txt", $"_CON{sep}x.txt", "_lpt1.log" }), "names Windows can't tell apart, or would read as devices, get their own");
Check(collidedTop.SequenceEqual(new[] { "docs", "Photos", "photos (2)", "_CON", "_lpt1.log" }), "each offered folder is one top-level item");
Check(Names.Safe("COM10") == "COM10" && Names.Safe("aux.txt") == "_aux.txt" && Names.Safe("console") == "console", "only real device names are changed");
Directory.Delete(dir, recursive: true);

// Setup check: the bridge has to be in both of PowerToys' lists.
System.Text.Json.JsonElement Props(string json) => System.Text.Json.JsonDocument.Parse(json).RootElement;
Check(SetupCheck.Problem(Props("{\"MachineMatrixString\":[\"alan-mac\",\"WINPC\",\"\",\"\"],\"MachinePool\":{\"value\":\"WINPC:1,alan-mac:2,:,:\"}}"), "Alan-Mac") == null, "a bridge in both lists is set up (names ignore case)");
Check(SetupCheck.Problem(Props("{\"MachineMatrixString\":{\"value\":[\"WINPC\",\"\",\"\",\"\"]},\"MachinePool\":{\"value\":\"WINPC:1,alan-mac:2,:,:\"}}"), "alan-mac")!.Contains("MachineMatrixString"), "missing from the layout is reported");
Check(SetupCheck.Problem(Props("{\"MachineMatrixString\":[\"alan-mac\",\"WINPC\",\"\",\"\"],\"MachinePool\":{\"value\":\"WINPC:1,:,:,:\"}}"), "alan-mac")!.Contains("MachinePool"), "missing from the pool is reported");
Check(SetupCheck.Problem(Props("{}"), "alan-mac") == null, "settings it can't read don't produce a false alarm");
Check(SetupCheck.Problem(Props("{\"MachineMatrixString\":[\"alan-mac \",\"WINPC\",\"\",\"\"],\"MachinePool\":{\"value\":\"WINPC:1, alan-mac:2,:,:\"}}"), "alan-mac") == null, "spaces PowerToys trims don't cause a false alarm");
Check(SetupCheck.Problem(Props("{\"MachineMatrixString\":[\"alan-mac\",\"WINPC\"],\"MachinePool\":{\"value\":\"WINPC:1,alan-mac:2,:,:\"}}"), "alan-mac")!.Contains("MachineMatrixString"), "a layout without four slots is one PowerToys ignores");
Check(SetupCheck.Problem(Props("{\"MachineMatrixString\":[\"alan-mac\",\"WINPC\",\"\",\"\"],\"MachinePool\":{\"value\":\"WINPC:1,alan-mac:2\"}}"), "alan-mac")!.Contains("four"), "a pool without four slots is one PowerToys ignores");
Check(SetupCheck.Problem(Props("{\"MachineMatrixString\":[\"alan-mac\",\"WINPC\",\"\",\"\"],\"MachinePool\":{\"value\":\"WINPC:1,alan-mac:NONE,:,:\"}}"), "alan-mac")!.Contains("valid ID"), "a pool entry without a usable ID is reported");
Check(SetupCheck.Problem(Props("{\"MachineMatrixString\":[\"alan-mac\",\"WINPC\",\"\",\"\"],\"MachinePool\":{\"value\":\"WINPC:1,:,:,:,alan-mac:2\"}}"), "alan-mac") != null, "only the first four pool slots count");

return failures == 0 ? 0 : 1;
