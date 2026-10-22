# System settings for running Superposition on NixOS: real-time limits, a low-latency kernel
# command line, the ntsync driver for Wine, the pinned Wine build, and Xvfb for the engine's
# private editor display.
#
# Users who run Superposition must be in the `audio` group.
{ wineFor }:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.superposition;
  # The flake's pinned Wine: licences in the shared prefix and the sp_wine_bridge ABI depend on it.
  wine = wineFor pkgs.stdenv.hostPlatform.system;
in
{
  options.programs.superposition = {
    enable = lib.mkEnableOption "the system settings the Superposition audio host needs";

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "Superposition build to install. Null installs only the runtime tools.";
    };

    lowLatencyKernel = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Boot with full kernel preemption and threaded interrupts (`preempt=full threadirqs`).
        On a stock desktop kernel, a real-time worker can still wait more than 1 ms to run, which
        misses blocks below 128 frames.
      '';
    };

    realtimePriority = lib.mkOption {
      type = lib.types.ints.between 1 99;
      default = 95;
      description = ''
        Highest SCHED_FIFO priority for the audio group. Superposition's processing threads use
        70 unless SUPERPOSITION_RT_PRIORITY changes it.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # SCHED_FIFO at Superposition's priority (70) needs the limit; rtkit grants at most 20.
    security.pam.loginLimits = [
      {
        domain = "@audio";
        type = "-";
        item = "rtprio";
        value = toString cfg.realtimePriority;
      }
    ];
    # The fallback for native threads in sessions without the login limits.
    security.rtkit.enable = true;
    # Faster Windows synchronisation in Wine builds that support it.
    boot.kernelModules = [ "ntsync" ];
    boot.kernelParams = lib.optionals cfg.lowLatencyKernel [
      "preempt=full"
      "threadirqs"
    ];

    environment.systemPackages = [
      wine
      pkgs.xorg-server # Xvfb
    ] ++ lib.optional (cfg.package != null) cfg.package;
    environment.sessionVariables.SUPERPOSITION_WINE = "${wine}/bin/wine";
  };
}
