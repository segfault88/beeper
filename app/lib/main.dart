import 'dart:async';
import 'dart:math';

import 'package:flutter/material.dart';
import 'package:flutter_blue_plus/flutter_blue_plus.dart';
import 'package:intl/intl.dart';
import 'package:permission_handler/permission_handler.dart';

// Must match firmware: little-endian 16-bit UUID 0xBEEF in Bluetooth base UUID
const String kServiceUuid = '0000beef-0000-1000-8000-00805f9b34fb';
// Must match firmware: characteristic UUID 0xBEE0
const String kTriggerCharUuid = '0000bee0-0000-1000-8000-00805f9b34fb';
const String kDeviceName = 'Beeper';

// RSSI to distance: d = 10 ^ ((txPower - rssi) / (10 * n))
// txPower: measured RSSI at 1m (~-59 dBm typical), n: path loss exponent (2.5 indoors)
double rssiToMeters(int rssi, {int txPower = -59, double n = 2.5}) {
  return pow(10.0, (txPower - rssi) / (10.0 * n)).toDouble();
}

void main() {
  FlutterBluePlus.setLogLevel(LogLevel.warning);
  runApp(const BeeperApp());
}

class BeeperApp extends StatelessWidget {
  const BeeperApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Beeper',
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(seedColor: Colors.deepOrange),
        useMaterial3: true,
      ),
      home: const ScanPage(),
    );
  }
}

// ---------------------------------------------------------------------------
// Scan page — discovers nearby Beeper devices
// ---------------------------------------------------------------------------

class ScanPage extends StatefulWidget {
  const ScanPage({super.key});

  @override
  State<ScanPage> createState() => _ScanPageState();
}

class _ScanPageState extends State<ScanPage> {
  final List<ScanResult> _results = [];
  bool _isScanning = false;
  StreamSubscription<List<ScanResult>>? _scanResultsSub;
  StreamSubscription<bool>? _isScanningSub;

  @override
  void initState() {
    super.initState();
    _isScanningSub = FlutterBluePlus.isScanning.listen((v) {
      if (mounted) setState(() => _isScanning = v);
    });
  }

  @override
  void dispose() {
    _scanResultsSub?.cancel();
    _isScanningSub?.cancel();
    super.dispose();
  }

  Future<void> _startScan() async {
    final statuses = await [
      Permission.bluetoothScan,
      Permission.bluetoothConnect,
      Permission.location,
    ].request();

    if (statuses.values.any((s) => s.isDenied)) {
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          const SnackBar(content: Text('Bluetooth permissions required')),
        );
      }
      return;
    }

    setState(() => _results.clear());

    _scanResultsSub?.cancel();
    _scanResultsSub = FlutterBluePlus.scanResults.listen((results) {
      if (!mounted) return;
      setState(() {
        for (final r in results) {
          final idx = _results.indexWhere(
            (e) => e.device.remoteId == r.device.remoteId,
          );
          if (idx >= 0) {
            _results[idx] = r;
          } else {
            _results.add(r);
          }
        }
        _results.sort((a, b) => b.rssi.compareTo(a.rssi));
      });
    });

    await FlutterBluePlus.startScan(
      withServices: [Guid(kServiceUuid)],
      timeout: const Duration(seconds: 15),
    );
  }

  Future<void> _stopScan() => FlutterBluePlus.stopScan();

  void _connectTo(BluetoothDevice device) {
    FlutterBluePlus.stopScan();
    Navigator.push(
      context,
      MaterialPageRoute(builder: (_) => DevicePage(device: device)),
    );
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('Beeper'),
        backgroundColor: Theme.of(context).colorScheme.inversePrimary,
        actions: [
          if (_isScanning)
            IconButton(icon: const Icon(Icons.stop), onPressed: _stopScan)
          else
            IconButton(icon: const Icon(Icons.search), onPressed: _startScan),
        ],
      ),
      body: _results.isEmpty
          ? Center(
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  const Icon(
                    Icons.bluetooth_searching,
                    size: 64,
                    color: Colors.grey,
                  ),
                  const SizedBox(height: 16),
                  Text(
                    _isScanning
                        ? 'Scanning…'
                        : 'Tap search to find Beeper devices',
                    style: Theme.of(context).textTheme.bodyLarge,
                  ),
                  if (_isScanning) ...[
                    const SizedBox(height: 16),
                    const CircularProgressIndicator(),
                  ],
                ],
              ),
            )
          : ListView.builder(
              itemCount: _results.length,
              itemBuilder: (context, i) {
                final r = _results[i];
                final name = r.advertisementData.advName.isNotEmpty
                    ? r.advertisementData.advName
                    : r.device.remoteId.str;
                final dist = rssiToMeters(r.rssi);
                return ListTile(
                  leading: const Icon(Icons.bluetooth),
                  title: Text(name),
                  subtitle: Text(
                    '${dist.toStringAsFixed(1)} m  •  ${r.rssi} dBm',
                  ),
                  trailing: const Icon(Icons.chevron_right),
                  onTap: () => _connectTo(r.device),
                );
              },
            ),
    );
  }
}

// ---------------------------------------------------------------------------
// Device page — connected view with distance readout and blink trigger
// ---------------------------------------------------------------------------

class DevicePage extends StatefulWidget {
  const DevicePage({super.key, required this.device});

  final BluetoothDevice device;

  @override
  State<DevicePage> createState() => _DevicePageState();
}

class _DevicePageState extends State<DevicePage> {
  BluetoothConnectionState _connState = BluetoothConnectionState.disconnected;
  BluetoothCharacteristic? _triggerChar;
  int? _rssi;
  DateTime? _lastSeen;
  bool _blinking = false;
  bool _connecting = false;

  StreamSubscription<BluetoothConnectionState>? _connSub;
  Timer? _rssiTimer;

  @override
  void initState() {
    super.initState();
    _connSub = widget.device.connectionState.listen((state) {
      if (mounted) setState(() => _connState = state);
      if (state == BluetoothConnectionState.connected) {
        _discoverServices();
        _startRssiPolling();
      } else {
        _rssiTimer?.cancel();
      }
    });
    _connect();
  }

  @override
  void dispose() {
    _rssiTimer?.cancel();
    _connSub?.cancel();
    widget.device.disconnect();
    super.dispose();
  }

  Future<void> _connect() async {
    setState(() => _connecting = true);
    try {
      await widget.device.connect(
        license: License.free,
        timeout: const Duration(seconds: 10),
      );
    } catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(content: Text('Connection failed: $e')),
        );
      }
    } finally {
      if (mounted) setState(() => _connecting = false);
    }
  }

  Future<void> _discoverServices() async {
    final services = await widget.device.discoverServices();
    for (final svc in services) {
      if (svc.uuid == Guid(kServiceUuid)) {
        for (final char in svc.characteristics) {
          if (char.uuid == Guid(kTriggerCharUuid)) {
            if (mounted) {
              setState(() {
                _triggerChar = char;
                _lastSeen = DateTime.now();
              });
            }
            return;
          }
        }
      }
    }
  }

  void _startRssiPolling() {
    _rssiTimer = Timer.periodic(const Duration(seconds: 2), (_) async {
      if (_connState != BluetoothConnectionState.connected) return;
      try {
        final rssi = await widget.device.readRssi();
        if (mounted) {
          setState(() {
            _rssi = rssi;
            _lastSeen = DateTime.now();
          });
        }
      } catch (_) {}
    });
  }

  Future<void> _triggerBlink() async {
    if (_triggerChar == null) return;
    setState(() => _blinking = true);
    try {
      await _triggerChar!.write([0x01], withoutResponse: true);
    } catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(content: Text('Write failed: $e')),
        );
      }
    }
    // Mirror the 3-second blink duration in the UI
    await Future.delayed(const Duration(seconds: 3));
    if (mounted) setState(() => _blinking = false);
  }

  String get _distanceText {
    if (_rssi == null) return '—';
    return '${rssiToMeters(_rssi!).toStringAsFixed(1)} m';
  }

  String get _lastSeenText {
    if (_lastSeen == null) return 'Never';
    return DateFormat('HH:mm:ss').format(_lastSeen!);
  }

  @override
  Widget build(BuildContext context) {
    final connected = _connState == BluetoothConnectionState.connected;
    final ready = connected && _triggerChar != null;

    return Scaffold(
      appBar: AppBar(
        title: Text(
          widget.device.platformName.isNotEmpty
              ? widget.device.platformName
              : kDeviceName,
        ),
        backgroundColor: Theme.of(context).colorScheme.inversePrimary,
      ),
      body: Padding(
        padding: const EdgeInsets.all(24),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            // Connection status
            Card(
              child: Padding(
                padding: const EdgeInsets.all(16),
                child: Row(
                  children: [
                    Icon(
                      connected
                          ? Icons.bluetooth_connected
                          : Icons.bluetooth_disabled,
                      color: connected ? Colors.green : Colors.grey,
                      size: 32,
                    ),
                    const SizedBox(width: 16),
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(
                            connected
                                ? 'Connected'
                                : (_connecting
                                    ? 'Connecting…'
                                    : 'Disconnected'),
                            style: Theme.of(context).textTheme.titleMedium,
                          ),
                          Text(
                            widget.device.remoteId.str,
                            style: Theme.of(context).textTheme.bodySmall,
                          ),
                        ],
                      ),
                    ),
                    if (_connecting)
                      const SizedBox(
                        width: 24,
                        height: 24,
                        child: CircularProgressIndicator(strokeWidth: 2),
                      ),
                  ],
                ),
              ),
            ),

            const SizedBox(height: 16),

            Row(
              children: [
                Expanded(
                  child: _StatCard(
                    label: 'Distance',
                    value: _distanceText,
                    icon: Icons.social_distance,
                  ),
                ),
                const SizedBox(width: 16),
                Expanded(
                  child: _StatCard(
                    label: 'RSSI',
                    value: _rssi != null ? '$_rssi dBm' : '—',
                    icon: Icons.signal_cellular_alt,
                  ),
                ),
              ],
            ),

            const SizedBox(height: 16),

            _StatCard(
              label: 'Last seen',
              value: _lastSeenText,
              icon: Icons.access_time,
            ),

            const Spacer(),

            FilledButton.icon(
              onPressed: ready && !_blinking ? _triggerBlink : null,
              icon: _blinking
                  ? const SizedBox(
                      width: 18,
                      height: 18,
                      child: CircularProgressIndicator(
                        color: Colors.white,
                        strokeWidth: 2,
                      ),
                    )
                  : const Icon(Icons.lightbulb),
              label: Text(_blinking ? 'Blinking…' : 'Blink LED'),
              style: FilledButton.styleFrom(
                minimumSize: const Size.fromHeight(56),
              ),
            ),

            const SizedBox(height: 8),

            if (!ready && !_connecting)
              Text(
                connected
                    ? 'Discovering Beeper service…'
                    : 'Connect to enable blink',
                textAlign: TextAlign.center,
                style: Theme.of(context).textTheme.bodySmall,
              ),
          ],
        ),
      ),
    );
  }
}

class _StatCard extends StatelessWidget {
  const _StatCard({
    required this.label,
    required this.value,
    required this.icon,
  });

  final String label;
  final String value;
  final IconData icon;

  @override
  Widget build(BuildContext context) {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Icon(icon, size: 18, color: Colors.grey),
                const SizedBox(width: 8),
                Text(
                  label,
                  style: Theme.of(context).textTheme.labelMedium,
                ),
              ],
            ),
            const SizedBox(height: 8),
            Text(value, style: Theme.of(context).textTheme.headlineSmall),
          ],
        ),
      ),
    );
  }
}
