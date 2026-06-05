{ config, lib, pkgs, ... }:

with lib;

let
  cfg = config.services.zenbook-duo-daemon;

  # Auto-detect product ID based on board name or use provided value
  effectiveProductId =
    if cfg.usbProductId == "auto" then
      # Default to 2024 model (most common/older model, more likely to be encountered)
      # The daemon auto-detects at runtime by reading /sys/class/dmi/id/board_name
      # so this is just a config file placeholder that works for both models
      "1b2c"  # Zenbook Duo 2024 (UX8406MA)
    else
      cfg.usbProductId;

  # Helper function to convert key function configuration to TOML format
  keyFunctionToToml = keyFunc:
    if keyFunc.type == "KeyBind" then
      if keyFunc.keys == [] then
        throw "KeyBind type requires at least one key in the 'keys' list"
      else
        "KeyBind = [${concatMapStringsSep ", " (k: ''"${k}"'') keyFunc.keys}]"
    else if keyFunc.type == "Command" then
      if keyFunc.command == "" then
        throw "Command type requires a non-empty 'command' string"
      else
        ''Command = "${keyFunc.command}"''
    else if keyFunc.type == "KeyboardBacklight" then
      "KeyboardBacklight = true"
    else if keyFunc.type == "ToggleSecondaryDisplay" then
      "ToggleSecondaryDisplay = true"
    else if keyFunc.type == "NoOp" then
      "NoOp = true"
    else
      throw "Unknown key function type: ${keyFunc.type}";

  # Generate config file content
  configFile = pkgs.writeText "zenbook-duo-daemon-config.toml" ''
    usb_vendor_id = "${cfg.usbVendorId}"
    usb_product_id = "${effectiveProductId}"
    secondary_display_status_path = "${cfg.secondaryDisplayStatusPath}"
    primary_backlight_path = "${cfg.primaryBacklightPath}"
    secondary_backlight_path = "${cfg.secondaryBacklightPath}"
    pipe_path = "${cfg.pipePath}"
    fn_lock = ${if cfg.fnLock then "true" else "false"}
    idle_timeout_seconds = ${toString cfg.idleTimeoutSeconds}

    [touchpad]
    enabled = ${if cfg.touchpad.enable then "true" else "false"}
    device_name = "${cfg.touchpad.deviceName}"
    left_border_percent = ${toString cfg.touchpad.leftBorderPercent}
    right_border_percent = ${toString cfg.touchpad.rightBorderPercent}
    top_border_percent = ${toString cfg.touchpad.topBorderPercent}
    double_tap_seek_seconds = ${toString cfg.touchpad.doubleTapSeekSeconds}
    brightness_step_percent = ${toString cfg.touchpad.brightnessStepPercent}
    volume_step_percent = ${toString cfg.touchpad.volumeStepPercent}
    brightness_increment_command = "${cfg.touchpad.brightnessIncrementCommand}"
    brightness_decrement_command = "${cfg.touchpad.brightnessDecrementCommand}"
    volume_increment_command = "${cfg.touchpad.volumeIncrementCommand}"
    volume_decrement_command = "${cfg.touchpad.volumeDecrementCommand}"
    player_list_interval_seconds = ${toString cfg.touchpad.playerListIntervalSeconds}
    player_position_interval_seconds = ${toString cfg.touchpad.playerPositionIntervalSeconds}

    [keyboard_backlight_key]
    ${keyFunctionToToml cfg.keyMappings.keyboardBacklight}

    [brightness_down_key]
    ${keyFunctionToToml cfg.keyMappings.brightnessDown}

    [brightness_up_key]
    ${keyFunctionToToml cfg.keyMappings.brightnessUp}

    [swap_up_down_display_key]
    ${keyFunctionToToml cfg.keyMappings.swapUpDownDisplay}

    [microphone_mute_key]
    ${keyFunctionToToml cfg.keyMappings.microphoneMute}

    [emoji_picker_key]
    ${keyFunctionToToml cfg.keyMappings.emojiPicker}

    [myasus_key]
    ${keyFunctionToToml cfg.keyMappings.myasus}

    [toggle_secondary_display_key]
    ${keyFunctionToToml cfg.keyMappings.toggleSecondaryDisplay}
  '';

  keyFunctionOption = types.submodule {
    options = {
      type = mkOption {
        type = types.enum [ "KeyBind" "Command" "KeyboardBacklight" "ToggleSecondaryDisplay" "NoOp" ];
        description = "Type of key function";
      };
      keys = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "List of keys for KeyBind type (e.g., [\"KEY_LEFTCTRL\" \"KEY_F10\"])";
      };
      command = mkOption {
        type = types.str;
        default = "";
        description = "Command to execute for Command type";
      };
    };
  };

in
{
  options.services.zenbook-duo-daemon = {
    enable = mkEnableOption "Zenbook Duo Daemon for ASUS Zenbook Duo laptops";

    package = mkOption {
      type = types.package;
      description = ''
        The zenbook-duo-daemon package to use.
        When using the module from the flake, pass the package explicitly:
          services.zenbook-duo-daemon.package = inputs.zenbook-duo-daemon.packages.$${pkgs.system}.default;
        Or use the overlay to make it available in pkgs.
      '';
      example = literalExpression "pkgs.zenbook-duo-daemon";
    };

    usbVendorId = mkOption {
      type = types.str;
      default = "0b05";
      description = "USB vendor ID of the keyboard (hex string)";
    };

    usbProductId = mkOption {
      type = types.str;
      default = "auto";
      description = ''
        USB product ID of the keyboard (hex string).
        Set to "auto" to use a sensible default (the daemon will auto-detect the correct ID at runtime).
        Or specify manually: "1bf2" for Zenbook Duo 2025 (UX8406CA) or "1b2c" for Zenbook Duo 2024 (UX8406MA).
      '';
    };

    secondaryDisplayStatusPath = mkOption {
      type = types.str;
      default = "/sys/class/drm/card1-eDP-2/status";
      description = "Path to secondary display status file";
    };

    primaryBacklightPath = mkOption {
      type = types.str;
      default = "/sys/class/backlight/intel_backlight/brightness";
      description = "Path to primary display backlight control";
    };

    secondaryBacklightPath = mkOption {
      type = types.str;
      default = "/sys/class/backlight/card1-eDP-2-backlight/brightness";
      description = "Path to secondary display backlight control";
    };

    pipePath = mkOption {
      type = types.str;
      default = "/run/zenbook-duo-daemon.pipe";
      description = "Path to the control pipe for sending commands to the daemon";
    };

    fnLock = mkOption {
      type = types.bool;
      default = true;
      description = "Whether Fn lock is enabled (true = F1-F12 require holding Fn)";
    };

    idleTimeoutSeconds = mkOption {
      type = types.int;
      default = 300;
      description = "Idle timeout in seconds before disabling keyboard backlight (0 to disable)";
    };

    touchpad = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "Enable touchpad border and corner gesture remapping";
      };

      deviceName = mkOption {
        type = types.str;
        default = "Primax Electronics Ltd. ASUS Zenbook Duo Keyboard Touchpad";
        description = "evdev device name of the keyboard touchpad";
      };

      leftBorderPercent = mkOption {
        type = types.float;
        default = 5.0;
        description = "Left touchpad border width as a percentage, used for brightness gestures";
      };

      rightBorderPercent = mkOption {
        type = types.float;
        default = 5.0;
        description = "Right touchpad border width as a percentage, used for volume gestures";
      };

      topBorderPercent = mkOption {
        type = types.float;
        default = 8.0;
        description = "Top touchpad border height as a percentage, used for media seeking and corner taps";
      };

      doubleTapSeekSeconds = mkOption {
        type = types.int;
        default = 10;
        description = "Number of seconds to seek on a double tap in a top corner";
      };

      brightnessStepPercent = mkOption {
        type = types.float;
        default = 5.0;
        description = "Gesture progress percentage required for one brightness command step";
      };

      volumeStepPercent = mkOption {
        type = types.float;
        default = 2.0;
        description = "Gesture progress percentage required for one volume command step";
      };

      brightnessIncrementCommand = mkOption {
        type = types.str;
        default = "qs -c $qsConfig ipc call brightness increment || brightnessctl s 5%+";
        description = "Command run as the active graphical user to increase brightness";
      };

      brightnessDecrementCommand = mkOption {
        type = types.str;
        default = "qs -c $qsConfig ipc call brightness decrement || brightnessctl s 5%-";
        description = "Command run as the active graphical user to decrease brightness";
      };

      volumeIncrementCommand = mkOption {
        type = types.str;
        default = "wpctl set-volume @DEFAULT_AUDIO_SINK@ 2%+ -l 1.5";
        description = "Command run as the active graphical user to increase volume";
      };

      volumeDecrementCommand = mkOption {
        type = types.str;
        default = "wpctl set-volume @DEFAULT_AUDIO_SINK@ 2%- -l 1.5";
        description = "Command run as the active graphical user to decrease volume";
      };

      playerListIntervalSeconds = mkOption {
        type = types.int;
        default = 5;
        description = "Interval for refreshing playerctl's MPRIS player list";
      };

      playerPositionIntervalSeconds = mkOption {
        type = types.int;
        default = 1;
        description = "Interval for polling MPRIS player status and position";
      };
    };

    keyMappings = {
      keyboardBacklight = mkOption {
        type = keyFunctionOption;
        default = { type = "KeyboardBacklight"; };
        description = "Keyboard backlight key mapping";
      };

      brightnessDown = mkOption {
        type = keyFunctionOption;
        default = { type = "KeyBind"; keys = [ "KEY_BRIGHTNESSDOWN" ]; };
        description = "Brightness down key mapping";
      };

      brightnessUp = mkOption {
        type = keyFunctionOption;
        default = { type = "KeyBind"; keys = [ "KEY_BRIGHTNESSUP" ]; };
        description = "Brightness up key mapping";
      };

      swapUpDownDisplay = mkOption {
        type = keyFunctionOption;
        default = { type = "NoOp"; };
        description = "Swap up/down display key mapping";
      };

      microphoneMute = mkOption {
        type = keyFunctionOption;
        default = { type = "KeyBind"; keys = [ "KEY_MICMUTE" ]; };
        description = "Microphone mute key mapping";
      };

      emojiPicker = mkOption {
        type = keyFunctionOption;
        default = { type = "KeyBind"; keys = [ "KEY_LEFTCTRL" "KEY_DOT" ]; };
        description = "Emoji picker key mapping";
      };

      myasus = mkOption {
        type = keyFunctionOption;
        default = { type = "NoOp"; };
        description = "MyASUS key mapping";
      };

      toggleSecondaryDisplay = mkOption {
        type = keyFunctionOption;
        default = { type = "ToggleSecondaryDisplay"; };
        description = "Toggle secondary display key mapping";
      };
    };
  };

  config = mkIf cfg.enable {
    # Install the package
    environment.systemPackages = [ cfg.package ];

    # Create the config directory and file
    environment.etc."zenbook-duo-daemon/config.toml".source = configFile;

    # Main daemon service
    systemd.services.zenbook-duo-daemon = {
      description = "Zenbook Duo Daemon";
      after = [ "sysinit.target" ];
      wantedBy = [ "multi-user.target" ];

      # Restart service when config changes
      restartIfChanged = true;

      serviceConfig = {
        Type = "simple";
        User = "root";
        Environment = "RUST_LOG=info";
        ExecStart = "${cfg.package}/bin/zenbook-duo-daemon run --config-path /etc/zenbook-duo-daemon/config.toml";
        Restart = "on-failure";
        RestartSec = 1;
        StandardOutput = "journal";
        StandardError = "journal";
      };

      # Rate limiting for restarts
      startLimitIntervalSec = 300;
      startLimitBurst = 5;
    };

    # Pre-sleep service
    systemd.services.zenbook-duo-daemon-pre-sleep = {
      description = "Notify Zenbook Duo daemon before sleep";
      before = [ "sleep.target" ];
      wantedBy = [ "sleep.target" ];

      # Restart service when config changes (e.g., pipe path)
      restartIfChanged = true;

      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.coreutils}/bin/timeout 1 ${pkgs.bash}/bin/bash -c 'echo suspend_start > ${cfg.pipePath} && sleep 0.5 || true'";
      };
    };

    # Post-sleep service
    systemd.services.zenbook-duo-daemon-post-sleep = {
      description = "Notify Zenbook Duo daemon after sleep";
      after = [ "suspend.target" "hibernate.target" "hybrid-sleep.target" "suspend-then-hibernate.target" ];
      wantedBy = [ "suspend.target" "hibernate.target" "hybrid-sleep.target" "suspend-then-hibernate.target" ];

      # Restart service when config changes (e.g., pipe path)
      restartIfChanged = true;

      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.coreutils}/bin/timeout 1 ${pkgs.bash}/bin/bash -c 'echo suspend_end > ${cfg.pipePath} || true'";
      };
    };
  };
}
