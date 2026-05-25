{ config, lib, pkgs, ... }:

let
  cfg = config.programs.shed;
in
{
  options.programs.shed = import ./shed_opts.nix { inherit pkgs lib; };

  config =
  lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."shed/shedrc".text = import ./render_rc.nix lib cfg;
  };
}
