{
  description = "UsageTracker daemon and CLI";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { nixpkgs, self }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          inherit (pkgs) lib;
          usageTracker = pkgs.rustPlatform.buildRustPackage {
            pname = "usagetracker";
            version = "0.1.5";
            src = self;

            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "--workspace" ];
            checkType = "debug";
            cargoTestFlags = [ "--workspace" ];

            postInstall = ''
              ln -s usage-cli $out/bin/usage
            '';

            meta = {
              description = "Local usage and rate-limit tracker for AI coding providers";
              homepage = "https://github.com/joeymalvinni/usagetracker";
              license = lib.licenses.mit;
              mainProgram = "usage-daemon";
              platforms = systems;
            };
          };
        in
        {
          default = usageTracker;
          daemon = usageTracker;
          cli = usageTracker;
        }
      );

      apps = forAllSystems (
        system:
        let
          package = self.packages.${system}.default;
        in
        {
          default = {
            type = "app";
            program = "${package}/bin/usage-daemon";
            meta.description = "Run the UsageTracker daemon";
          };
          daemon = {
            type = "app";
            program = "${package}/bin/usage-daemon";
            meta.description = "Run the UsageTracker daemon";
          };
          cli = {
            type = "app";
            program = "${package}/bin/usage";
            meta.description = "Query UsageTracker from the command line";
          };
        }
      );

      checks = forAllSystems (system: {
        package = self.packages.${system}.default;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];
            packages = with pkgs; [
              cargo
              clippy
              just
              rustc
              rustfmt
            ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
