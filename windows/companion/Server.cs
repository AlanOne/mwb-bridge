using System.Net;
using System.Net.Sockets;

namespace MwbBridgeCompanion;

/// Accepts the bridge's connections: one long-lived control connection,
/// plus a short data connection per file transfer.
sealed class Server
{
    readonly Config _config;
    readonly Coordinator _coordinator;
    readonly Action<string?> _status;
    readonly Action<string> _warn;
    // Bridges already warned about, so a reconnecting bridge doesn't warn
    // again each time.
    readonly HashSet<string> _warned = new(StringComparer.OrdinalIgnoreCase);
    FrameConnection? _control;
    readonly object _controlLock = new();

    public Server(Config config, Coordinator coordinator, Action<string?> status, Action<string> warn)
    {
        _config = config;
        _coordinator = coordinator;
        _status = status;
        _warn = warn;
        coordinator.SendToBridge = Send;
        coordinator.BridgeConnected = () => { lock (_controlLock) return _control != null; };
    }

    public void Start()
    {
        var listener = new TcpListener(IPAddress.IPv6Any, _config.Port);
        listener.Server.DualMode = true;
        listener.Start();
        Log.Write($"Listening on port {_config.Port} for the bridge.");
        new Thread(() =>
        {
            while (true)
            {
                try
                {
                    var client = listener.AcceptTcpClient();
                    new Thread(() => Serve(client)) { IsBackground = true }.Start();
                }
                catch (Exception e)
                {
                    // One failed accept mustn't end the server.
                    Log.Write($"Accepting a connection failed: {e.Message}");
                    Thread.Sleep(200);
                }
            }
        }) { IsBackground = true }.Start();
    }

    static readonly TimeSpan HelloTimeout = TimeSpan.FromSeconds(10);
    static readonly TimeSpan ControlSendTimeout = TimeSpan.FromSeconds(2);
    // A transfer that stalls this long (a peer gone mid-file) is given up.
    static readonly TimeSpan DataTimeout = TimeSpan.FromSeconds(30);

    /// The same keepalive as the bridge's: probes after 15s of quiet, every
    /// 5s, giving up after 3.
    static void EnableKeepAlive(Socket socket)
    {
        try
        {
            socket.SetSocketOption(SocketOptionLevel.Socket, SocketOptionName.KeepAlive, true);
            socket.SetSocketOption(SocketOptionLevel.Tcp, SocketOptionName.TcpKeepAliveTime, 15);
            socket.SetSocketOption(SocketOptionLevel.Tcp, SocketOptionName.TcpKeepAliveInterval, 5);
            socket.SetSocketOption(SocketOptionLevel.Tcp, SocketOptionName.TcpKeepAliveRetryCount, 3);
        }
        catch (Exception e)
        {
            Log.Write($"Couldn't turn on keepalive: {e.Message}");
        }
    }

    bool Send(uint type, object message)
    {
        lock (_controlLock)
        {
            if (_control == null) return false;
            try
            {
                _control.WriteJson(type, message);
                return true;
            }
            catch (Exception e)
            {
                Log.Write($"Sending to the bridge failed: {e.Message}");
                return false;
            }
        }
    }

    void Serve(TcpClient client)
    {
        var peer = client.Client.RemoteEndPoint;
        FrameConnection? conn = null;
        try
        {
            client.NoDelay = true;
            // Something that connects and then says nothing (or isn't the
            // bridge) mustn't hold a thread forever.
            client.ReceiveTimeout = client.SendTimeout = (int)HelloTimeout.TotalMilliseconds;
            conn = FrameConnection.Open(client, _config.SecurityKey ?? throw new IOException("no security key"));
            var hello = conn.ReadJson<HelloMsg>(Frames.Hello);
            if (hello.Version != Frames.Version)
            {
                Log.Write($"The bridge speaks companion protocol version {hello.Version}, this app {Frames.Version}: update {(hello.Version < Frames.Version ? "the bridge" : "this app")} so both come from the same version of the repo.");
            }
            if (hello.Role == "control")
            {
                // Quiet for long stretches: no read timeout, but keepalive
                // notices a bridge that vanished (a Mac gone to sleep).
                client.ReceiveTimeout = 0;
                client.SendTimeout = (int)ControlSendTimeout.TotalMilliseconds;
                EnableKeepAlive(client.Client);
                RunControl(conn, hello, peer);
            }
            else if (hello.Role == "data" && hello.Id is ulong id)
            {
                client.ReceiveTimeout = client.SendTimeout = (int)DataTimeout.TotalMilliseconds;
                RunData(conn, hello.Op, id, client);
            }
        }
        catch (Exception e)
        {
            Log.Write($"Connection from {peer} ended: {e.Message}");
        }
        finally
        {
            conn?.Dispose();
            client.Dispose();
        }
    }

    void RunControl(FrameConnection conn, HelloMsg hello, EndPoint? peer)
    {
        lock (_controlLock)
        {
            _control?.Dispose(); // a reconnect replaces the old connection
            _control = conn;
        }
        Log.Write($"Bridge {hello.Name} connected from {peer}.");
        _status(hello.Name);
        if (Config.MwbSetupProblem(hello.Name) is string problem)
        {
            Log.Write(problem);
            bool first;
            lock (_warned) first = _warned.Add(hello.Name);
            if (first) _warn(problem);
        }
        try
        {
            while (true)
            {
                var (type, body) = conn.ReadFrame();
                switch (type)
                {
                    case Frames.Active:
                        _coordinator.Post(_coordinator.OnActive);
                        break;
                    case Frames.Inactive:
                        var inactive = FrameConnection.Parse<InactiveMsg>(body);
                        _coordinator.Post(() => _coordinator.OnInactive(inactive, hello.Version));
                        break;
                    case Frames.OfferToWindows:
                        // Registered here, not on the UI thread, so the
                        // files' data connection can't arrive before it. A
                        // drop that can't be laid out on disk fails alone,
                        // not the whole connection.
                        try
                        {
                            var drop = _coordinator.Register(FrameConnection.Parse<OfferMsg>(body));
                            _coordinator.Post(() => _coordinator.OnOfferToWindows(drop));
                        }
                        catch (Exception e) when (e is IOException or UnauthorizedAccessException or NotSupportedException or ArgumentException)
                        {
                            Log.Write($"[to-windows] Couldn't take a drag from the bridge: {e.Message}");
                        }
                        break;
                    case Frames.ClipboardToWindows:
                        try
                        {
                            _coordinator.RegisterCopy(FrameConnection.Parse<OfferMsg>(body));
                        }
                        catch (Exception e) when (e is IOException or UnauthorizedAccessException or NotSupportedException or ArgumentException)
                        {
                            Log.Write($"[copy] Couldn't take files copied on the bridge: {e.Message}");
                        }
                        break;
                    case Frames.Cancel:
                        // Its transfer is cut here, at once; its drag (if
                        // any) ends on the UI thread.
                        var cancel = FrameConnection.Parse<CancelMsg>(body);
                        Log.Write($"The bridge cancelled transfer {cancel.Id}.");
                        Transfers.Cancel(cancel.Id);
                        _coordinator.Post(() => _coordinator.OnCancel(cancel.Id));
                        break;
                    default:
                        Log.Write($"Ignoring control frame {type}.");
                        break;
                }
            }
        }
        finally
        {
            // A reconnect replaced this connection: the new one is live, so
            // its status and its drags are left alone.
            bool current;
            lock (_controlLock)
            {
                current = _control == conn;
                if (current) _control = null;
            }
            if (current)
            {
                _status(null);
                _coordinator.Post(_coordinator.OnBridgeDisconnected);
                Log.Write($"Bridge {hello.Name} disconnected.");
            }
            else
            {
                Log.Write($"Bridge {hello.Name}'s previous connection ended (replaced by a reconnect).");
            }
        }
    }

    /// Stops every transfer going on (the tray's "Cancel transfer"): tells
    /// the bridge first, so it takes the closed connection for a cancel,
    /// then cuts it here.
    public void CancelAll()
    {
        foreach (var t in Transfers.Snapshot())
        {
            Log.Write($"Cancelling transfer {t.Id}.");
            Send(Frames.Cancel, new CancelMsg(t.Id));
            Transfers.Cancel(t.Id);
            _coordinator.Post(() => _coordinator.OnCancel(t.Id));
        }
    }

    void RunData(FrameConnection conn, string? op, ulong id, TcpClient client)
    {
        if (op == "get")
        {
            var files = _coordinator.FilesToMac(id) ?? throw new IOException($"no drag {id} to send");
            long total = 0;
            foreach (var f in files)
            {
                try { total += new FileInfo(f.Path).Length; } catch (Exception) { }
            }
            var transfer = Transfers.Begin(id, true, total, client.Close) ?? throw new IOException($"transfer {id} was cancelled");
            try
            {
                conn.SendFiles(files, transfer.Add);
            }
            catch (Exception) when (transfer.Cancelled)
            {
                throw new IOException($"transfer {id} was cancelled");
            }
            finally
            {
                Transfers.End(transfer);
            }
            Log.Write($"[to-mac] Sent drag {id}'s {files.Length} file(s).");
        }
        else if (op == "put")
        {
            // The offer comes over the control connection; give it a moment.
            IncomingDrop? drop = null;
            for (var i = 0; i < 40 && (drop = _coordinator.FromMac(id)) == null; i++) Thread.Sleep(50);
            if (drop == null) throw new IOException($"no drag {id} to receive");
            var transfer = Transfers.Begin(id, false, drop.Total, client.Close);
            if (transfer == null)
            {
                drop.Failed = true;
                throw new IOException($"transfer {id} was cancelled");
            }
            try
            {
                conn.ReceiveFiles(h => drop.Paths[h.Index], drop.FileDone, transfer.Add);
            }
            catch (Exception) when (transfer.Cancelled)
            {
                drop.Failed = true;
                throw new IOException($"transfer {id} was cancelled");
            }
            catch
            {
                drop.Failed = true;
                throw;
            }
            finally
            {
                Transfers.End(transfer);
            }
        }
    }
}
