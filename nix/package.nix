{ lib, stdenvNoCC, fetchurl }:
let
  release = builtins.fromJSON (builtins.readFile ./release.json);
  artifact = release.platforms.${stdenvNoCC.hostPlatform.system};
in stdenvNoCC.mkDerivation {
  pname = "sigla";
  inherit (release) version;
  src = fetchurl { inherit (artifact) url hash; };
  dontUnpack = true;
  dontStrip = true;
  installPhase = ''
    runHook preInstall
    install -Dm755 "$src" "$out/bin/sigla"
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
