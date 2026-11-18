using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

namespace Ccync.Verification
{
    public static class CalibrationControlRunner
    {
        public static int Main(string[] arguments)
        {
            if (arguments.Length != 6) return 2;
            IDisposable pipeline = null;
            try
            {
                Assembly automation = Assembly.LoadFrom(arguments[0]);
                Type powershell = automation.GetType("System.Management.Automation.PowerShell", true);
                object instance = powershell.GetMethod("Create", Type.EmptyTypes).Invoke(null, null);
                pipeline = (IDisposable)instance;
                powershell.GetMethod("AddCommand", new Type[] { typeof(string) }).Invoke(instance, new object[] { arguments[1] });
                MethodInfo parameter = powershell.GetMethod("AddParameter", new Type[] { typeof(string), typeof(object) });
                string[] names = { "Control", "ResultPath", "ArgumentValue", "UnicodeArgument" };
                for (int index = 0; index < names.Length; index++)
                    parameter.Invoke(instance, new object[] { names[index], arguments[index + 2] });
                MethodInfo invoke = Array.Find(powershell.GetMethods(), delegate(MethodInfo method)
                    { return method.Name == "Invoke" && !method.IsGenericMethod && method.GetParameters().Length == 0; });
                invoke.Invoke(instance, null);
                if ((bool)powershell.GetProperty("HadErrors").GetValue(instance, null))
                {
                    Console.Error.WriteLine("The calibration PowerShell pipeline reported errors.");
                    return 1;
                }
                return 0;
            }
            catch (Exception exception)
            {
                Console.Error.WriteLine(exception.GetBaseException().Message);
                return 1;
            }
            finally { if (pipeline != null) pipeline.Dispose(); }
        }
    }

    public sealed class ProcessIdentity
    {
        public uint pid;
        public uint parentPid;
        public long creationFileTime;
        public long parentCreationFileTime;
        public string image;
    }

    public sealed class WindowSample
    {
        public long hwnd;
        public uint pid;
        public long creationFileTime;
        public bool visible;
        public bool attributed;
        public string windowClass;
        public long elapsedMilliseconds;
    }

    public sealed class WindowObservation
    {
        public string desktop;
        public string executable;
        public string[] arguments;
        public string commandLine;
        public string cwd;
        public string failedStep;
        public string error;
        public bool executionStarted;
        public bool observationComplete;
        public bool parentChainComplete;
        public bool jobClosed;
        public bool rootExited;
        public uint exitCode;
        public int polls;
        public int anchorObservations;
        public long observerWindow;
        public uint observerThreadId;
        public long maxPollingGapMilliseconds;
        public readonly List<ProcessIdentity> parentChain = new List<ProcessIdentity>();
        public readonly List<WindowSample> events = new List<WindowSample>();
        public readonly List<WindowSample> observerEvents = new List<WindowSample>();
        public readonly List<ObservedProcessCleanup> cleanup = new List<ObservedProcessCleanup>();
        public readonly List<string> cleanupErrors = new List<string>();
    }

    public sealed class ObservedProcessCleanup
    {
        public ProcessIdentity identity;
        public bool stopped;
        public string error;
    }

    public sealed class WindowsObserver : IDisposable
    {
        private IntPtr desktop;
        private IntPtr anchorWindow;
        private uint anchorThreadId;
        private uint observerPid;
        private long observerCreated;
        private Thread anchorThread;
        private readonly ManualResetEvent anchorReady = new ManualResetEvent(false);
        private readonly ManualResetEvent anchorStop = new ManualResetEvent(false);
        private string anchorError;
        public string DesktopName { get; private set; }
        public string WindowStation { get; private set; }
        public bool DesktopClosed { get; private set; }
        public string CloseError { get; private set; }
        private static readonly IntPtr InvalidHandle = new IntPtr(-1);

        public WindowsObserver()
        {
            if (Environment.OSVersion.Platform != PlatformID.Win32NT)
                throw new PlatformNotSupportedException("InteractiveWindowStation: Windows is required.");
            uint session;
            if (!ProcessIdToSessionId((uint)Process.GetCurrentProcess().Id, out session) || session == 0)
                throw new InvalidOperationException("InteractiveWindowStation: an interactive session is required.");
            IntPtr station = GetProcessWindowStation();
            UserObjectFlags flags;
            uint needed;
            if (station == IntPtr.Zero || !GetUserObjectFlags(station, 1, out flags, 12, out needed))
                throw Failure("InteractiveWindowStation");
            if ((flags.flags & 1) == 0)
                throw new InvalidOperationException("InteractiveWindowStation: the window station is not visible.");
            StringBuilder stationName = new StringBuilder(256);
            if (!GetUserObjectName(station, 2, stationName, 512, out needed))
                throw Failure("WindowStationName");
            WindowStation = stationName.ToString();
            observerPid = (uint)Process.GetCurrentProcess().Id;
            observerCreated = CreationTime(observerPid);
            DesktopName = "ccync-calibration-" + Guid.NewGuid().ToString("N");
            // Omit DESKTOP_SWITCHDESKTOP so this handle cannot activate the desktop.
            desktop = CreateDesktop(DesktopName, null, IntPtr.Zero, 0, 0x0001 | 0x0002 | 0x0040 | 0x0080, IntPtr.Zero);
            if (desktop == IntPtr.Zero) throw Failure("CreateDesktop");
            try
            {
                // A hidden observer window makes successful enumeration distinguishable from an empty-list FALSE.
                anchorThread = new Thread(delegate()
                {
                    try
                    {
                        if (!SetThreadDesktop(desktop)) throw Failure("ObserverThreadDesktop");
                        anchorThreadId = GetCurrentThreadId();
                        anchorWindow = CreateWindowEx(0, "STATIC", "ccync-calibration-observer", 0,
                            0, 0, 0, 0, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero);
                        if (anchorWindow == IntPtr.Zero) throw Failure("ObserverAnchorWindow");
                        anchorReady.Set();
                        anchorStop.WaitOne();
                    }
                    catch (Exception exception) { anchorError = exception.Message; }
                    finally
                    {
                        if (anchorWindow != IntPtr.Zero && !DestroyWindow(anchorWindow))
                            anchorError = Failure("DestroyObserverAnchor").Message;
                        anchorReady.Set();
                    }
                });
                anchorThread.IsBackground = true;
                anchorThread.Start();
                if (!anchorReady.WaitOne(5000)) throw new TimeoutException("ObserverAnchorWindow: initialization timed out.");
                if (anchorError != null) throw new InvalidOperationException(anchorError);
            }
            catch { Dispose(); throw; }
        }

        public WindowObservation RunControl(string executable, string[] arguments, string cwd,
            Dictionary<string, string> environment, string stdinPath, string stdoutPath, string stderrPath,
            int timeoutMilliseconds)
        {
            WindowObservation result = new WindowObservation();
            result.desktop = WindowStation + "\\" + DesktopName;
            result.executable = executable;
            result.arguments = arguments;
            result.cwd = cwd;
            result.observerWindow = anchorWindow.ToInt64();
            result.observerThreadId = anchorThreadId;
            string step = "DesktopPreflight";
            IntPtr job = IntPtr.Zero, input = InvalidHandle, output = InvalidHandle, error = InvalidHandle;
            IntPtr attributes = IntPtr.Zero, handles = IntPtr.Zero;
            IntPtr environmentBlock = IntPtr.Zero;
            bool attributesInitialized = false;
            ProcessInformation process = new ProcessInformation();
            try
            {
                if (desktop == IntPtr.Zero) throw new InvalidOperationException("Owned desktop is closed.");
                if (timeoutMilliseconds < 1000 || timeoutMilliseconds > 30000)
                    throw new ArgumentOutOfRangeException("timeoutMilliseconds");
                step = "CreateJobObject";
                job = CreateJobObject(IntPtr.Zero, null);
                if (job == IntPtr.Zero) throw Failure(step);
                JobExtendedLimit limits = new JobExtendedLimit();
                limits.basic.limitFlags = 0x2000;
                if (!SetInformationJobObject(job, 9, ref limits, (uint)Marshal.SizeOf(limits))) throw Failure(step);
                step = "StandardHandles";
                SecurityAttributes security = new SecurityAttributes();
                security.length = Marshal.SizeOf(security);
                security.inheritHandle = true;
                input = CreateFile(stdinPath, 0x80000000, 3, ref security, 3, 0x80, IntPtr.Zero);
                output = CreateFile(stdoutPath, 0x40000000, 3, ref security, 2, 0x80, IntPtr.Zero);
                error = CreateFile(stderrPath, 0x40000000, 3, ref security, 2, 0x80, IntPtr.Zero);
                if (input == InvalidHandle || output == InvalidHandle || error == InvalidHandle) throw Failure(step);
                step = "InheritedHandleList";
                IntPtr size = IntPtr.Zero;
                InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref size);
                attributes = Marshal.AllocHGlobal(size);
                if (!InitializeProcThreadAttributeList(attributes, 1, 0, ref size)) throw Failure(step);
                attributesInitialized = true;
                handles = Marshal.AllocHGlobal(IntPtr.Size * 3);
                Marshal.WriteIntPtr(handles, 0, input);
                Marshal.WriteIntPtr(handles, IntPtr.Size, output);
                Marshal.WriteIntPtr(handles, IntPtr.Size * 2, error);
                if (!UpdateProcThreadAttribute(attributes, 0, new IntPtr(0x20002), handles,
                    new IntPtr(IntPtr.Size * 3), IntPtr.Zero, IntPtr.Zero)) throw Failure(step);
                StartupInfoEx startup = new StartupInfoEx();
                startup.startup.cb = Marshal.SizeOf(startup);
                startup.startup.desktop = result.desktop;
                startup.startup.flags = 0x100;
                startup.startup.stdInput = input;
                startup.startup.stdOutput = output;
                startup.startup.stdError = error;
                startup.attributes = attributes;
                StringBuilder command = new StringBuilder(Quote(executable));
                foreach (string argument in arguments) command.Append(" ").Append(Quote(argument));
                result.commandLine = command.ToString();
                List<string> keys = new List<string>(environment.Keys);
                keys.Sort(StringComparer.OrdinalIgnoreCase);
                StringBuilder block = new StringBuilder();
                foreach (string key in keys) block.Append(key).Append('=').Append(environment[key]).Append('\0');
                block.Append('\0');
                environmentBlock = Marshal.StringToHGlobalUni(block.ToString());
                step = "CreateProcess";
                // The control host uses the Windows GUI subsystem so AllocConsole is not suppressed by launch flags.
                if (!CreateProcess(executable, command, IntPtr.Zero, IntPtr.Zero, true,
                    0x00000004 | 0x00000400 | 0x00080000, environmentBlock, cwd, ref startup, out process))
                    throw Failure(step);
                result.executionStarted = true;
                step = "AssignProcessToJobObject";
                if (!AssignProcessToJobObject(job, process.process)) throw Failure(step);
                step = "RootParentIdentity";
                Dictionary<uint, ProcessRow> before = Snapshot();
                ProcessRow rootRow;
                if (!before.TryGetValue(process.pid, out rootRow)) throw new InvalidOperationException("Root is missing from native process snapshot.");
                uint callerPid = (uint)Process.GetCurrentProcess().Id;
                long callerCreated = CreationTime(callerPid);
                long rootCreated = CreationTime(process.process);
                if (rootRow.parentPid != callerPid || callerCreated == 0 || rootCreated < callerCreated)
                    throw new InvalidOperationException("Root native parent identity does not match this observer.");
                ProcessIdentity root = new ProcessIdentity { pid = process.pid, parentPid = callerPid,
                    creationFileTime = rootCreated, parentCreationFileTime = callerCreated, image = rootRow.image };
                Dictionary<uint, ProcessIdentity> owned = new Dictionary<uint, ProcessIdentity>();
                owned.Add(root.pid, root);
                result.parentChain.Add(root);
                result.parentChainComplete = true;
                step = "ResumeThread";
                if (ResumeThread(process.thread) == UInt32.MaxValue) throw Failure(step);
                Stopwatch watch = Stopwatch.StartNew();
                long previous = 0;
                while (true)
                {
                    step = "ProcessParentObservation";
                    AdoptDescendants(owned, result);
                    step = "EnumDesktopWindows";
                    long elapsed = watch.ElapsedMilliseconds;
                    result.maxPollingGapMilliseconds = Math.Max(result.maxPollingGapMilliseconds, elapsed - previous);
                    previous = elapsed;
                    SampleWindows(result, owned, elapsed);
                    uint wait = WaitForSingleObject(process.process, 0);
                    if (wait == 0)
                    {
                        step = "GetExitCodeProcess";
                        if (!GetExitCodeProcess(process.process, out result.exitCode)) throw Failure(step);
                        result.rootExited = true;
                        result.observationComplete = true;
                        break;
                    }
                    if (wait != 258) throw Failure("WaitForSingleObject");
                    if (watch.ElapsedMilliseconds >= timeoutMilliseconds)
                    {
                        step = "ControlTimeout";
                        throw new TimeoutException("Control did not exit within its observation bound.");
                    }
                    Thread.Sleep(10);
                }
            }
            catch (Exception exception)
            {
                result.failedStep = step;
                result.error = exception.Message;
                result.observationComplete = false;
            }
            finally
            {
                if (process.process != IntPtr.Zero && !result.rootExited)
                {
                    // The suspended root is ours even if job assignment failed.
                    TerminateProcess(process.process, 1);
                    bool exited = WaitForSingleObject(process.process, 5000) == 0;
                    if (!exited) result.cleanupErrors.Add("Owned root termination was not confirmed.");
                }
                if (job != IntPtr.Zero) result.jobClosed = CloseHandle(job);
                if (job != IntPtr.Zero && !result.jobClosed) result.cleanupErrors.Add(Failure("CloseOwnedJob").Message);
                foreach (ProcessIdentity identity in result.parentChain)
                    result.cleanup.Add(CheckCleanup(identity));
                CloseNative(process.thread);
                CloseNative(process.process);
                CloseNative(input); CloseNative(output); CloseNative(error);
                if (attributesInitialized) DeleteProcThreadAttributeList(attributes);
                if (attributes != IntPtr.Zero) Marshal.FreeHGlobal(attributes);
                if (handles != IntPtr.Zero) Marshal.FreeHGlobal(handles);
                if (environmentBlock != IntPtr.Zero) Marshal.FreeHGlobal(environmentBlock);
            }
            return result;
        }

        private void SampleWindows(WindowObservation result, Dictionary<uint, ProcessIdentity> owned, long elapsed)
        {
            string failure = null;
            bool anchorObserved = false;
            EnumWindow callback = delegate(IntPtr hwnd, IntPtr context)
            {
                uint pid;
                uint threadId = GetWindowThreadProcessId(hwnd, out pid);
                if (threadId == 0)
                {
                    failure = "GetWindowThreadProcessId: window disappeared or attribution failed.";
                    return false;
                }
                if (hwnd == anchorWindow)
                {
                    if (pid != observerPid || threadId != anchorThreadId || IsWindowVisible(hwnd))
                    {
                        failure = "Observer anchor identity or hidden state changed.";
                        return false;
                    }
                    anchorObserved = true;
                    return true;
                }
                long created = CreationTime(pid);
                ProcessIdentity identity;
                bool attributed = owned.TryGetValue(pid, out identity) && created != 0 && identity.creationFileTime == created;
                bool visible = IsWindowVisible(hwnd);
                StringBuilder windowClass = new StringBuilder(256);
                if (GetClassName(hwnd, windowClass, windowClass.Capacity) == 0)
                {
                    failure = "GetClassName: window observation failed.";
                    return false;
                }
                bool observerInfrastructure = pid == observerPid && threadId == anchorThreadId && created == observerCreated &&
                    !visible && (windowClass.ToString() == "IME" || windowClass.ToString() == "MSCTFIME UI");
                List<WindowSample> samples = observerInfrastructure ? result.observerEvents : result.events;
                WindowSample last = samples.FindLast(delegate(WindowSample sample)
                    { return sample.hwnd == hwnd.ToInt64() && sample.pid == pid && sample.creationFileTime == created; });
                if (last == null || last.visible != visible || last.attributed != attributed)
                {
                    if (result.events.Count + result.observerEvents.Count >= 2000) { failure = "Window sample capacity exceeded."; return false; }
                    samples.Add(new WindowSample { hwnd = hwnd.ToInt64(), pid = pid, creationFileTime = created,
                        visible = visible, attributed = attributed, windowClass = windowClass.ToString(), elapsedMilliseconds = elapsed });
                }
                return true;
            };
            SetLastError(0);
            if (!EnumDesktopWindows(desktop, callback, IntPtr.Zero))
                throw new InvalidOperationException(failure ?? Failure("EnumDesktopWindows").Message);
            if (!anchorObserved) throw new InvalidOperationException("Observer anchor is absent from desktop enumeration.");
            result.anchorObservations++;
            result.polls++;
        }

        private static void AdoptDescendants(Dictionary<uint, ProcessIdentity> owned, WindowObservation result)
        {
            Dictionary<uint, ProcessRow> rows = Snapshot();
            bool added;
            do
            {
                added = false;
                foreach (ProcessRow row in rows.Values)
                {
                    if (owned.ContainsKey(row.pid)) continue;
                    ProcessIdentity parent;
                    if (!owned.TryGetValue(row.parentPid, out parent)) continue;
                    if (CreationTime(parent.pid) != parent.creationFileTime) continue;
                    long created = CreationTime(row.pid);
                    if (created == 0) throw new InvalidOperationException("Descendant creation identity is unavailable.");
                    if (created < parent.creationFileTime) continue;
                    ProcessIdentity child = new ProcessIdentity { pid = row.pid, parentPid = row.parentPid,
                        creationFileTime = created, parentCreationFileTime = parent.creationFileTime, image = row.image };
                    owned.Add(child.pid, child);
                    result.parentChain.Add(child);
                    added = true;
                }
            } while (added);
        }

        private static Dictionary<uint, ProcessRow> Snapshot()
        {
            IntPtr snapshot = CreateToolhelp32Snapshot(2, 0);
            if (snapshot == InvalidHandle) throw Failure("CreateToolhelp32Snapshot");
            try
            {
                ProcessEntry entry = new ProcessEntry();
                entry.size = (uint)Marshal.SizeOf(entry);
                if (!Process32First(snapshot, ref entry)) throw Failure("Process32First");
                Dictionary<uint, ProcessRow> rows = new Dictionary<uint, ProcessRow>();
                do { rows[entry.pid] = new ProcessRow { pid = entry.pid, parentPid = entry.parentPid, image = entry.image }; }
                while (Process32Next(snapshot, ref entry));
                if (Marshal.GetLastWin32Error() != 18) throw Failure("Process32Next");
                return rows;
            }
            finally { CloseHandle(snapshot); }
        }

        private static long CreationTime(uint pid)
        {
            IntPtr process = OpenProcess(0x1000, false, pid);
            if (process == IntPtr.Zero) return 0;
            try { return CreationTime(process); }
            finally { CloseHandle(process); }
        }

        private static long CreationTime(IntPtr process)
        {
            long created, exited, kernel, user;
            return GetProcessTimes(process, out created, out exited, out kernel, out user) ? created : 0;
        }

        private static ObservedProcessCleanup CheckCleanup(ProcessIdentity identity)
        {
            ObservedProcessCleanup cleanup = new ObservedProcessCleanup { identity = identity, error = "" };
            IntPtr process = OpenProcess(0x1000 | 0x100000, false, identity.pid);
            if (process == IntPtr.Zero)
            {
                int code = Marshal.GetLastWin32Error();
                cleanup.stopped = code == 87;
                if (!cleanup.stopped) cleanup.error = "Cleanup identity check failed (Win32 " + code + ")";
                return cleanup;
            }
            try
            {
                long created = CreationTime(process);
                cleanup.stopped = created != 0 && (created != identity.creationFileTime || WaitForSingleObject(process, 1000) == 0);
                if (!cleanup.stopped) cleanup.error = "Owned process termination was not confirmed.";
                return cleanup;
            }
            finally { CloseHandle(process); }
        }

        private static string Quote(string value)
        {
            StringBuilder quoted = new StringBuilder("\"");
            int slashes = 0;
            foreach (char character in value)
            {
                if (character == '\\') { slashes++; continue; }
                quoted.Append('\\', slashes * (character == '"' ? 2 : 1) + (character == '"' ? 1 : 0));
                quoted.Append(character);
                slashes = 0;
            }
            return quoted.Append('\\', slashes * 2).Append('"').ToString();
        }

        private static Win32Exception Failure(string step)
        {
            int code = Marshal.GetLastWin32Error();
            return new Win32Exception(code, step + " failed (Win32 " + code + ")");
        }
        private static void CloseNative(IntPtr handle) { if (handle != IntPtr.Zero && handle != InvalidHandle) CloseHandle(handle); }

        public void Dispose()
        {
            if (desktop == IntPtr.Zero) return;
            anchorStop.Set();
            if (anchorThread != null && !anchorThread.Join(5000))
            {
                CloseError = "ObserverAnchorWindow: cleanup timed out.";
                return;
            }
            DesktopClosed = CloseDesktop(desktop);
            if (!DesktopClosed) CloseError = Failure("CloseDesktop").Message;
            else desktop = IntPtr.Zero;
            if (anchorError != null) { DesktopClosed = false; CloseError = anchorError; }
        }

        private sealed class ProcessRow { public uint pid; public uint parentPid; public string image; }
        private delegate bool EnumWindow(IntPtr hwnd, IntPtr context);
        [StructLayout(LayoutKind.Sequential)] private struct UserObjectFlags { public int inherit; public int reserved; public uint flags; }
        [StructLayout(LayoutKind.Sequential)] private struct SecurityAttributes { public int length; public IntPtr descriptor; [MarshalAs(UnmanagedType.Bool)] public bool inheritHandle; }
        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] private struct StartupInfo
        {
            public int cb; public string reserved; public string desktop; public string title;
            public uint x, y, xSize, ySize, xCountChars, yCountChars, fillAttribute, flags;
            public short showWindow, reservedSize; public IntPtr reservedBytes, stdInput, stdOutput, stdError;
        }
        [StructLayout(LayoutKind.Sequential)] private struct StartupInfoEx { public StartupInfo startup; public IntPtr attributes; }
        [StructLayout(LayoutKind.Sequential)] private struct ProcessInformation { public IntPtr process, thread; public uint pid, threadId; }
        [StructLayout(LayoutKind.Sequential)] private struct JobBasicLimit
        {
            public long processTime, jobTime; public uint limitFlags; public UIntPtr minimumWorkingSet, maximumWorkingSet;
            public uint activeProcesses; public UIntPtr affinity; public uint priority, scheduling;
        }
        [StructLayout(LayoutKind.Sequential)] private struct IoCounters { public ulong readOperations, writeOperations, otherOperations, readBytes, writeBytes, otherBytes; }
        [StructLayout(LayoutKind.Sequential)] private struct JobExtendedLimit { public JobBasicLimit basic; public IoCounters io; public UIntPtr processMemory, jobMemory, peakProcessMemory, peakJobMemory; }
        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] private struct ProcessEntry
        {
            public uint size, usage, pid; public UIntPtr defaultHeap; public uint module, threads, parentPid;
            public int priority; public uint flags; [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string image;
        }
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateDesktopW")]
        private static extern IntPtr CreateDesktop(string name, string device, IntPtr mode, uint flags, uint access, IntPtr security);
        [DllImport("user32.dll", SetLastError = true)] private static extern bool CloseDesktop(IntPtr desktop);
        [DllImport("user32.dll", SetLastError = true)] private static extern bool SetThreadDesktop(IntPtr desktop);
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateWindowExW")]
        private static extern IntPtr CreateWindowEx(uint extendedStyle, string windowClass, string title, uint style,
            int x, int y, int width, int height, IntPtr parent, IntPtr menu, IntPtr instance, IntPtr parameter);
        [DllImport("user32.dll", SetLastError = true)] private static extern bool DestroyWindow(IntPtr window);
        [DllImport("user32.dll", SetLastError = true)] private static extern IntPtr GetProcessWindowStation();
        [DllImport("user32.dll", SetLastError = true, EntryPoint = "GetUserObjectInformationW")]
        private static extern bool GetUserObjectFlags(IntPtr handle, int index, out UserObjectFlags flags, uint size, out uint needed);
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "GetUserObjectInformationW")]
        private static extern bool GetUserObjectName(IntPtr handle, int index, StringBuilder name, uint size, out uint needed);
        [DllImport("user32.dll", SetLastError = true)] private static extern bool EnumDesktopWindows(IntPtr desktop, EnumWindow callback, IntPtr context);
        [DllImport("user32.dll")] private static extern bool IsWindowVisible(IntPtr window);
        [DllImport("user32.dll", SetLastError = true)] private static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "GetClassNameW")]
        private static extern int GetClassName(IntPtr window, StringBuilder name, int size);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool ProcessIdToSessionId(uint pid, out uint session);
        [DllImport("kernel32.dll")] private static extern uint GetCurrentThreadId();
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateProcessW")]
        private static extern bool CreateProcess(string application, StringBuilder command, IntPtr processSecurity, IntPtr threadSecurity,
            bool inheritHandles, uint flags, IntPtr environment, string cwd, ref StartupInfoEx startup, out ProcessInformation process);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern uint ResumeThread(IntPtr thread);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool TerminateProcess(IntPtr process, uint exitCode);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool CloseHandle(IntPtr handle);
        [DllImport("kernel32.dll")] private static extern void SetLastError(uint error);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool GetProcessTimes(IntPtr process, out long created, out long exited, out long kernel, out long user);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateJobObjectW")]
        private static extern IntPtr CreateJobObject(IntPtr security, string name);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool SetInformationJobObject(IntPtr job, int type, ref JobExtendedLimit limits, uint size);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool InitializeProcThreadAttributeList(IntPtr list, int count, int flags, ref IntPtr size);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern bool UpdateProcThreadAttribute(IntPtr list, uint flags, IntPtr attribute, IntPtr value, IntPtr size, IntPtr previous, IntPtr returned);
        [DllImport("kernel32.dll")] private static extern void DeleteProcThreadAttributeList(IntPtr list);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateFileW")]
        private static extern IntPtr CreateFile(string path, uint access, uint share, ref SecurityAttributes security, uint disposition, uint flags, IntPtr template);
        [DllImport("kernel32.dll", SetLastError = true)] private static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint pid);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "Process32FirstW")]
        private static extern bool Process32First(IntPtr snapshot, ref ProcessEntry entry);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "Process32NextW")]
        private static extern bool Process32Next(IntPtr snapshot, ref ProcessEntry entry);
    }
}
