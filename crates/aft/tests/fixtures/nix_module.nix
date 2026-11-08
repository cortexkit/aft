{ config, lib, pkgs, ... }:
{
  options.services.demo.enable = lib.mkEnableOption "demo";

  config.services.demo = {
    enable = true;
    port = 80;
  };
}
