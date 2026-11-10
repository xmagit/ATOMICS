{ lib, rustPlatform, pkg-config, patchelf, alsa-lib, wayland, libxkbcommon
, vulkan-loader, mesa, fontconfig, git, libx11, libxcursor, libxrandr, libxi, makeWrapper }:

let
  runtimeLibraries = [
    alsa-lib
    wayland
    libxkbcommon
    vulkan-loader
    mesa
    fontconfig
    libx11
    libxcursor
    libxrandr
    libxi
  ];
  root = ../.;
  # Git-tracked files only, so build output, local state and anything untracked beside a checkout
  # never reach the store. gitTracked needs the working tree itself. A flake is evaluated from a
  # store copy instead: of the tracked files for a git flake (`nix build .`), or of the whole
  # folder, .git included, for a path: flake (`nix build path:.`), and gitTracked refuses a store
  # path. There the untracked folders are dropped by name.
  tracked =
    if !lib.path.hasStorePathPrefix root && builtins.pathExists (root + "/.git") then
      lib.fileset.gitTracked root
    else
      lib.fileset.difference root
        (lib.fileset.unions (map (name: lib.fileset.maybeMissing (root + "/${name}")) [ ".git" ".atomics" "target" ]));
  # Of those, what the build reads: docs, Nix files and unrelated scripts stay out of the source.
  # The revision belongs to the separate wrapper, so a docs-only commit reuses this Cargo build.
  buildInputsOnly = lib.fileset.intersection tracked (lib.fileset.unions [
    (root + "/Cargo.toml")
    (root + "/Cargo.lock")
    (root + "/crates")
    (root + "/assets")
    # The guest check `atomics vm setup` runs in the VM.
    (root + "/scripts/check-atomics-vm.sh")
    # Shared test support, which test builds of the crates name.
    (root + "/tests")
  ]);
in
rustPlatform.buildRustPackage {
  pname = "atomics";
  # The workspace's own version, so the store path names the release it holds.
  version = (lib.importTOML ../Cargo.toml).workspace.package.version;
  src = lib.fileset.toSource { inherit root; fileset = buildInputsOnly; };

  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "-p" "atomics" ];
  doCheck = false;

  nativeBuildInputs = [ pkg-config patchelf makeWrapper ];
  buildInputs = runtimeLibraries;

  # The window, audio and font libraries ATOMICS opens at runtime go in its own RUNPATH, not in
  # LD_LIBRARY_PATH: agent terminals inherit ATOMICS's environment, and a library path there would
  # load these libraries into every program an agent runs. After fixup, which shrinks the RUNPATH
  # to the libraries the binary links directly.
  postFixup = ''
    patchelf --add-rpath "${lib.makeLibraryPath runtimeLibraries}" "$out/bin/.atomics-wrapped"
  '';

  # git is a fallback after the user's own PATH, so agent terminals keep finding their git first.
  postInstall = ''
    wrapProgram "$out/bin/atomics" --suffix PATH : "${lib.makeBinPath [ git ]}"
  '';

  meta = {
    description = "Review-first workspace for coding agents";
    homepage = "https://github.com/xmagit/ATOMICS";
    license = lib.licenses.mit;
    mainProgram = "atomics";
    platforms = [ "aarch64-linux" "x86_64-linux" ];
  };
}
