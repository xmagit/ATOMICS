# Revision metadata must not become an input of the expensive Cargo derivation.
{ lib, runCommand, makeWrapper, atomics, revision ? null }:

runCommand "atomics-${atomics.version}" {
  inherit (atomics) pname version meta;
  nativeBuildInputs = [ makeWrapper ];
  passthru.unwrapped = atomics;
} ''
  mkdir -p "$out/bin" "$out/share/atomics"
  ln -s ${atomics}/share/polkit-1 "$out/share/polkit-1"
  cat > "$out/share/atomics/build.json" <<'JSON'
  ${builtins.toJSON { binary = "${atomics}/bin/.atomics-wrapped"; inherit revision; }}
  JSON
  makeWrapper ${atomics}/bin/atomics "$out/bin/atomics" --set ATOMICS_PACKAGE "$out"
  test "$("$out/bin/atomics" --version)" = ${lib.escapeShellArg "atomics ${atomics.version} (${if revision == null then atomics.version else revision})"}
''
