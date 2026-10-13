{ lib, stdenvNoCC, fetchurl, makeWrapper, coreutils, buildPackages }:
let
  release = builtins.fromJSON (builtins.readFile ./release.json);
  artifact = release.platforms.${stdenvNoCC.hostPlatform.system};
in stdenvNoCC.mkDerivation {
  pname = "sigla";
  inherit (release) version;
  src = fetchurl { inherit (artifact) url hash; };
  debugSrc = fetchurl { inherit (artifact.debug) url hash; };
  dontUnpack = true;
  dontStrip = true;
  nativeBuildInputs = [ makeWrapper buildPackages.binutils ];
  installPhase = ''
    runHook preInstall
    install -Dm755 "$src" "$out/bin/sigla"
    debugName="$(${buildPackages.binutils.targetPrefix}readelf --string-dump=.gnu_debuglink "$src" | awk '/\[ *0\]/ { print $NF }')"
    test -n "$debugName"
    install -Dm644 "$debugSrc" "$out/bin/$debugName"
    wrapProgram "$out/bin/sigla" --prefix PATH : ${lib.makeBinPath [ coreutils ]}
    runHook postInstall
  '';
  meta = {
    description = "Shared C# and Rust source navigation over MCP";
    homepage = "https://github.com/apkd/Sigla";
    license = lib.licenses.mit;
    platforms = builtins.attrNames release.platforms;
    mainProgram = "sigla";
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
}
