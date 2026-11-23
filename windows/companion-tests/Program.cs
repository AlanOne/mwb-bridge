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
var (t1, b1) = server.ReadFrame();
Check(t1 == Frames.Active && b1.Length == 0, "an empty frame arrives empty");
Check(server.ReadJson<InactiveMsg>(Frames.Inactive).ButtonHeld, "JSON uses the bridge's snake_case names");

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
