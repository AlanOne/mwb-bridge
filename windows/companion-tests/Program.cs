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
var sent = Task.Run(() => client.SendFiles(new[] { Path.Combine(dir, "big.bin"), Path.Combine(dir, "empty.txt") }));
var done = new List<int>();
server.ReceiveFiles(h => Path.Combine(dir, "got-" + h.Name), done.Add);
await sent;
Check(File.ReadAllBytes(Path.Combine(dir, "got-big.bin")).SequenceEqual(big), "a file bigger than a chunk arrives intact");
Check(File.Exists(Path.Combine(dir, "got-empty.txt")) && new FileInfo(Path.Combine(dir, "got-empty.txt")).Length == 0, "an empty file arrives");
Check(done.SequenceEqual(new[] { 0, 1 }), "each file is reported done, in order");
Directory.Delete(dir, recursive: true);

return failures == 0 ? 0 : 1;
