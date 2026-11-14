# ADR 0011: Plug-in workers in a Windows VM

- **Status:** Accepted for Lambda's appliance; gated by Lambda's phase V0 measurements
- **Date:** 2026-11-06

## Context

Some Windows VST3 plug-ins do not run under Wine, first those protected by iLok. Lambda
(`plans/2026-11-06-windows-plugin-vm.html` in the Lambda repository) runs them in a Windows
guest under KVM on the same Linux host, as normal racks: the same lane, gate, fallback and
replacement rules, and the same worker.

## Decision

- **A VM port per rack.** A VM port is one `ivshmem-doorbell` PCI device, served by its own
  port server (`sp-ivshmem`, a Rust implementation of QEMU's ivshmem protocol). Its memory
  holds a header page and one rack's two banks (`create_port_banks`). The lane's wake is one
  eventfd write to the guest's doorbell, which KVM delivers as an MSI. No guest process copies
  audio.
- **One worker build.** The Windows worker opens a bank by name. A name `vmport-N-bank-B`
  opens port N's IVSHMEM device (PCI 00:(0x10+N).0, mapped cached) and waits on its doorbell
  event; any other name is a Wine bank. Under Wine without the bridge the worker still fails
  early. A process shares one device handle per port, because the driver lets one handle map
  a device.
- **Two clock domains.** The guest's clock is the performance counter. A worker in the VM
  writes its claim and completion ticks anchored at the host's publication tick
  (`MonotonicClock::slot_tick`), so the protocol's order checks hold and no guest tick is
  compared with a host tick. The host's deadline decides late or fault.
- **A guest agent in the user's session** (`sp-vm-agent`). It starts and stops one worker per
  port, relays each worker's control pipes over a virtio-serial port, scans the guest's VST3
  folder, runs uploaded installers, and sends a heartbeat. It runs at logon in the auto-logon
  user's session, so editors and installers show on the VM screen; a session-0 service would
  need a launch into the user's session. Ports use overlapped I/O: Windows serialises reads
  and writes on a synchronous handle.
- **Threads off the real-time vCPUs.** The agent sets the worker's process mask to the
  Windows vCPUs and the port's real-time vCPU; a TLS thread-attach callback moves every new
  thread to the Windows vCPUs; the audio thread moves itself to the real-time vCPU at
  time-critical priority. This lowers the risk; a plug-in can still set its own affinity.

## Consequences

The host needs no Windows code. A crashed guest, worker or agent is the rack's fallback,
never a host fault. The port server must outlive QEMU's connection and must not reuse a
departed peer's ID soon: QEMU 10.2's ivshmem writes into freed memory when an ID returns.
Hooks on the Win32 scheduling calls that plug-ins make are not built. Whether a guest meets
32 frames at 96 kHz is Lambda's phase V0 question, measured on isolated cores.
