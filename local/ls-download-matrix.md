# serena @43ae0211 — A 类单二进制下载 LS 下载矩阵（26 项，marksman 跳过 → 实收 25）

来源：各适配器 .py（raw @43ae0211）+ GitHub releases API assets[].digest（实调）+ 官方校验端点。调研日期 2026-09-20。

⚠️ owner/repo 勘误（Main 清单 vs 源码实际）：ada-core/AdaLanguageServer→**AdaCore/ada_language_server**；borkdude/bsl→**1c-syntax/bsl-language-server**；elixir-tools/elixir-tools-vscode→**elixir-lang/expert（已 302 至 expert-lsp/expert）**；ananthakumaran/vshaxe→**Open VSX nadako/vshaxe（非 GitHub）**；shader-sense/shader-language-server→**antaalt/shader-sense**；Kotlin/kotlin-server-release→**download-cdn.jetbrains.com（非 GitHub）**；sumneko/lua-language-server→**LuaLS/lua-language-server**；mathworks/MATLAB-language-server→**Marketplace MathWorks.language-matlab（非 GitHub）**；OmniSharp/omnisharp-roslyn→**roslynomnisharp.blob.core.windows.net（非 GitHub releases）**；ghostdogpr/calmrun→不存在，nextflow 为 **nextflow-io/language-server**；phpactor/phpantom→**PHPantom-dev/phpantom_lsp**；hashicorp/terraform-ls→**releases.hashicorp.com（非 GitHub）**；ebkalderon/texlab→**latex-lsp/texlab**；dart-lang/sdk→**storage.googleapis.com/dart-archive（非 GitHub）**；eclipse-jdtls/eclipse.jdt.ls→serena 实下 **redhat-developer/vscode-java VSIX**。

图例：**pin**=上游 DEFAULT_* 固定版；**latest**=实测最新稳定；digest 标记 [API]=本次实调 assets[].digest、[embed]=serena 适配器/hash-DB 内嵌、[官方✓]=与发布方校验文件一致。平台：W=win-x64 Wa=win-arm64 L=linux-x64 La=linux-aarch64 M=macos-x86_64 Ma=macos-aarch64。

## 快览

| LS | pin | latest | digest 来源 | 缺口 |
|---|---|---|---|---|
| ada | 2026.2.202604091 | 2026.3.202607051（日期 MISSING） | embed | latest digest |
| al | 18.0.2242655 | MISSING | embed | latest 版本+digest |
| bsl | 0.29.0 | v1.0.7（2026-08-08） | embed+[API] | — |
| csharp(roslyn) | 5.5.0-2.26078.4 | 5.11.0（另有 5.12.0-1.26426.8 预发） | embed | latest digest |
| clojure | 2026.02.20-16.08.58 | 2026.07.06-14.34.19（2026-07-06） | embed+[API] | latest W digest |
| cue | v0.16.1 | v0.17.1 | embed | latest digest |
| dart | 3.7.1 | 3.13.4（2026-09-15） | embed | latest digest |
| eclipse_jdtls | VSIX 1.54.0-923+Gradle 8.14.2 | VSIX v1.56.0 | hash-DB+[官方✓gradle] | latest VSIX digest |
| elixir | v0.1.0-rc.6 | v0.1.10（2026-09-08，仓库迁移） | embed+[API] | — |
| haxe | 2.34.2 | 2.34.2（=pin，2025-10-17） | embed+OpenVSX sidecar | — |
| hlsl | 1.3.1 | v1.5.0（2026-09-07） | embed+[API] | — |
| kotlin | 262.9593.0 | MISSING | hash-DB | latest 版本+digest |
| lua | 3.15.0 | 3.19.1（日期 MISSING） | embed | latest digest |
| luau | 1.63.0 | 1.69.0（2026-07-18） | embed+[API] | — |
| matlab | 1.3.9 | MISSING | embed | latest 版本+digest |
| nextflow | 26.04.3 | v26.04.4（2026-09-16） | hash-DB+[API] | — |
| omnisharp | 1.39.10+razor 7.0.0-preview.23363.1 | blob 上即 1.39.10 | JSON integrity | — |
| pascal | v0.2.0 | v0.2.0（=pin，2026-01-17） | embed=[API]=[官方✓] | — |
| phpactor | 2025.12.21.1 | 2026.06.23.0（2026-08-15） | embed+[API] | — |
| phpantom | 0.8.0 | 0.10.0（2026-08-20） | embed+[API] | — |
| powershell | 4.4.0 | v4.7.0（2026-06-25） | embed+[API] | — |
| systemverilog | v0.0-4051-g9fdb4057 | v0.0-4283-ga1b0580b（日期 MISSING） | embed | latest digest |
| toml | 0.10.0 | 0.10.0（=pin，2025-05-23） | 仅 embed（GitHub digest=null） | — |
| terraform | 0.36.5 | 未查询 | embed=[官方✓] | latest 版本 |
| latex | 5.25.1 | v5.26.0（2026-06-25） | embed+[API] | latest M/W digest |

---

## 1. ada

- pin `2026.2.202604091`；latest `2026.3.202607051`（日期 MISSING）
- URL: `https://github.com/AdaCore/ada_language_server/releases/download/{v}/als-{v}-{suffix}.tar.gz`；suffix: W=`win32-x64` L=`linux-x64` La=`linux-arm64` M=`darwin-x64` Ma=`darwin-arm64`；**Wa 无资产**
- archive: tar.gz｜bin: `integration/vscode/ada/{x64|arm64}/{win32|linux|darwin}/ada_language_server[.exe]`｜exec: 无参（stdio）
- sha256 [embed]: W `bca024dc3643b2d91aebbb747398e2e6f183ad8cb2f149e24fe7a38cdb16ed8d`｜L `2eb436a7c0e3740128cceaa15da9ff856fa75ef3fbb3e2e9d3a1bd17e15cb949`｜La `65c57df715df90f7581ecd6a1d1884663376baaf38cedbab80d10060ff91b03d`｜M `18d3277a25a6e08ce3ee7230c3e5ac20419d573cb8d8c883e7983f387a67d223`｜Ma `1d25ded29b6beafcb34c9d0084d52809d84577356e467bf92e8fef32fc4216c4`

## 2. al — VSIX 形态，需独立安装器设计

- pin `18.0.2242655`；latest MISSING（Marketplace 需 POST 查询）
- URL 规律: `https://marketplace.visualstudio.com/_apis/public/gallery/publishers/ms-dynamics-smb/vsextensions/al/{v}/vspackage`（VSIX=zip 多平台合一）
- bin: `extension/bin/{win32|linux|darwin}/Microsoft.Dynamics.Nav.EditorServices.Host[.exe]`｜exec: 无参
- sha256 [embed] VSIX 整体: `3971995e61a59dc4fcce4a65053072a67991ed624a16635c4f2911f12564b2b9`（Marketplace 无官方 digest）

## 3. bsl

- pin `0.29.0`；latest `v1.0.7`（2026-08-08）
- URL: `https://github.com/1c-syntax/bsl-language-server/releases/download/v{v}/bsl-language-server-{v}-exec.jar`｜any（JAR, JVM≥21）
- archive: raw｜bin: JAR 本体｜exec: `java -jar <jar>`
- sha256: pin [embed] `d6fa9ad638ba51855e260b88ad1f8ce4e602385845a4ee43600d148f779bcf0b`；latest [API] `9f62765edd344d66456da24c906eaf623a03c56e90e5aafee466200100909f64`
- 注: v1.0.x 另有自带 JBR 原生包 `bsl-language-server_{win|nix|mac}.zip`（[API] `fed523e3…`/`af6ba018…`/`c889da05…`，serena 未用）

## 4. csharp（Roslyn, NuGet）

- pin `5.5.0-2.26078.4`；NuGet index 实测：最新无预发后缀 `5.11.0`（最新条目 `5.12.0-1.26426.8`）
- URL: `https://www.nuget.org/api/v2/package/roslyn-language-server.{plat}/{v}`；plat: `win-x64/win-arm64/osx-x64/osx-arm64/linux-x64/linux-arm64`（全 6 平台）
- archive: nupkg（zip）｜bin: `tools/net10.0/{plat}/Microsoft.CodeAnalysis.LanguageServer.dll`｜exec: `dotnet <dll> --logLevel=Information --extensionLogDirectory=<dir> --stdio`（.NET 10+）
- sha256 [embed]: W `7f3d4119e75305399e6faa81a68240b33c48b94ad523a904594abd00db95572a`｜Wa `0fe3381c4340a7494a5242c3d0c8be1af6ef0802de8b458f947cebca76fd26bc`｜M `c8de61a88c65150e12f561a2659f70b59d27a7465865136a1de950d2ef826c6d`｜Ma `995207c14e01dafa71e84080a7eb1f045a697b0c3bb468077bb3809b69bdf456`｜L `1aad25de456d637a1eee993ca0d569a1b78d711744ccb36410a3a20250a48aa6`｜La `a7dd49bbc0e25d0e2968ae31ec5f3c774373866db51f3500fcea0ac320e2bbc1`；NuGet 无官方 sha256 端点（registration 仅 SHA-512）

## 5. clojure

- pin `2026.02.20-16.08.58`；latest `2026.07.06-14.34.19`（2026-07-06）
- URL: `https://github.com/clojure-lsp/clojure-lsp/releases/download/{v}/clojure-lsp-native-{plat}.zip`；plat: Ma=`macos-aarch64` M=`macos-amd64` La=`linux-aarch64` L=`linux-amd64` W=`windows-amd64`
- archive: zip｜bin: 根 `clojure-lsp[.exe]`｜exec: 无参（需 Clojure CLI）
- sha256: pin [embed] W `817b1271288817c954fb9e595278b1f25003827ce31f8785f253dc4ac911041f`｜L `52e8bf4fd4cf171df0a3077c8bb5a3bf598d4c621e94b4876dab943a61267309`｜La `f8f09fa07dd4b6743b5c57270ccf1ee5cdbc5fca09dbca8b6a3b22705b5da4e1`｜M `5507434c27104ab816e096d3336d8191641de8a65b57d76afb585d07167a3cf2`｜Ma `a14d4db074f665378214e2dc888472e186c228dfa065c777b0534bfda5571669`；latest [API] L `520f724ee02f4b3ecb225395a7a5a4ccad3878d6d1418240cd9636afcf9b858e`｜La `0595e65a5934d3208246f529b5cf0497d7167d7e9b8317e9b391e05b5c0906d7`｜M `0449f7f8fc975157cb4e5cdcf365bcd43bcf1fa47b99256427e7a86e4c17fc3f`｜Ma `dd9a8e36add53b8d8166bb3d7580c6e5563401aea87b62600786af2e7d37ccde`｜W MISSING（该 release 附 .sha256 sidecar）

## 6. cue

- pin `v0.16.1`；latest 稳定 `v0.17.1`（v0.18.0-alpha.* 为预发）
- URL: `https://github.com/cue-lang/cue/releases/download/{v}/cue_v{v去v}_{os}_{arch}.{ext}`：M `darwin_amd64.tar.gz` Ma `darwin_arm64.tar.gz` L `linux_amd64.tar.gz` La `linux_arm64.tar.gz` W `windows_amd64.zip` **Wa `windows_arm64.zip`**
- archive: unix=tar.gz win=zip｜bin: 根 `cue[.exe]`｜exec: `cue lsp`
- sha256 [embed]: W `2f24123f458229fcf283db534bd86692ad1074da806defee0f0cc62976c0397c`｜Wa `e0c15ce53f73e8609b0e8ce6507298f3474b334ac5eb0c826c9497a811fd0cce`｜L `5d644c1305a2b86504c8dcd2ec829cf5b4999efc2cf51ee375624e0455f774ae`｜La `3cc715a9e969f87b93c4fa34cfaef5388b93e96efa20b248e8ad6826abd25a83`｜M `97b0d78e4c5ee49ff72145fd6ef4f4bab0bb332d55f29660de3fec2af5ec96a9`｜Ma `a72b0cddb377c52d1b003bed9a335d893b70cd75a182cd5e3fee8bae30ddb6d6`

## 7. dart

- pin `3.7.1`；latest `3.13.4`（2026-09-15，VERSION JSON 实调）
- URL: `https://storage.googleapis.com/dart-archive/channels/stable/release/{v}/sdk/dartsdk-{plat}-release.zip`；plat: L=`linux-x64` W=`windows-x64` Wa=`windows-arm64` M=`macos-x64` Ma=`macos-arm64`（无 La）
- archive: zip（整 SDK）｜bin: `dart-sdk/bin/dart[.exe]`｜exec: `dart language-server --client-id multilspy.dart --client-version 1.2`
- sha256 [embed]: L `2813959e7d9650334015b927cc533f5beadfbf7fa48248beec471f8942a0ee71`｜W `f56c03122e17abe5be1429eee0a975fb8ed511b6731ec90c6475992d3dee4ea5`｜Wa `fada411c6538d0ac24c35d6360767241f1298f64cbc5e88716387d54757a105a`｜M `a2765917b6ae49d1ac119553df9584989f9c441a46e8f18c129ba52489658d2e`｜Ma `f57c25163092bac818f8ca6250a0d8b2c56344c6a075a1bd7c60da7ac28b32a4`；latest digest MISSING

## 8. eclipse_jdtls（VSIX + Gradle 双下载）

- pin VSIX `1.54.0-923`（tag v1.54.0）+ Gradle `8.14.2`；latest VSIX `v1.56.0`
- URL: `https://github.com/redhat-developer/vscode-java/releases/download/v{base}/java-{plat}-{v}.vsix`（plat: `darwin-arm64/darwin-x64/linux-arm64/linux-x64/win32-x64`）+ `https://services.gradle.org/distributions/gradle-{v}-bin.zip`
- archive: zip｜bin: `extension/jre/21.0.10-{macosx-aarch64|macosx-x86_64|linux-aarch64|linux-x86_64|win32-x86_64}/bin/java[.exe]`、`extension/server/plugins/org.eclipse.equinox.launcher_1.7.100.v20251111-0406.jar`、`extension/server/config_{mac_arm|mac|linux_arm|linux|win}`、`extension/lombok/lombok-1.18.39-4050.jar`｜exec: 打包 JRE21 启动 equinox launcher（JDK≥21）
- sha256 [hash-DB]: gradle `7197a12f450794931532469d4ff21a59ea2c1cd59a3ec3f89c035c3c420a6999`（=官方 .sha256 实调一致✓）；VSIX: Ma `c54c45cb0d2579d8e0a4ddeb24d4a9dd0b460d07d9366adea2b38a1da22a463c`｜M `dfc98abc4e54165a78372e280242a039671729b1b03420608df3b10c6b629fb6`｜La `e2bb22c427d90da8dbb1afff72ff1e2dce38d50b76deb02d7bc313a330a1330c`｜L `9d4b15da54e25a0192f9bac073f086c015397d3676623b68dbf83a5dbaf5132b`｜W `66f3914987edeccfee8a2558470e0fde4f8c4154232ff4baa5d73373ebc819d4`

## 9. elixir（Expert）

- pin `v0.1.0-rc.6`；latest `v0.1.10`（2026-09-08）。⚠️ 仓库迁移 elixir-lang/expert → **expert-lsp/expert**（API 302 实测）
- URL: `https://github.com/expert-lsp/expert/releases/download/{v}/expert_{os}_{arch}`：L `linux_amd64` La `linux_arm64` M `darwin_amd64` Ma `darwin_arm64` W `windows_amd64.exe`（无 Wa）
- archive: raw binary｜bin: 重命名 `expert[.exe]`｜exec: `expert --stdio`（需 Elixir）
- sha256: pin [embed] L `643a492ff972246668b0ca356a84c3d0a0f5feeae0ab5dc1b9a126876ed460e4`｜La `d8b830bdaa8991d7ebf255dacbb3674f3ea335c87d0bfba4b7f907ded4a8f014`｜M `964f316f1633090b33aab392b6b85fb778c5fb3c0db862671424458da34b1d4d`｜Ma `5fb5be151baedd635d99835cf3f9986afc9af6ae7b07bd001a1962f4298e45da`｜W `babee77d2653679021600b99c68d984d4463290cb221e0fc0d1093b3afdeb3b0`；latest [API] L `af29d5270503263139f10fccb597430feb0c510da48386821b176fbfdfe33f8d`｜La `eaac1a779dc8319099576edcd0c43dde3a383abf2e75ccd6989cdabf3900cd9b`｜M `5cb885b7e83e73c9c8fc16c27f398391311629113a0a6e26134e5ae4b7005035`｜Ma `992a7c6c0ad062667d6518ca378c62fe3a465d3f57350d5123028741195f1e1a`｜W `163cb83a75316d77068fd1d00eb43e113dd2bb2d417af48d3e5374f6763a5880`；v0.1.10 起有 `expert_checksums.txt` sidecar

## 10. haxe（Open VSX）— VSIX 形态，需独立安装器设计

- pin `2.34.2` = latest（2025-10-17，Open VSX API 实调）
- URL: `https://open-vsx.org/api/nadako/vshaxe/{v}/file/nadako.vshaxe-{v}.vsix`（universal VSIX=zip）
- bin: `bin/server.js`｜exec: `node server.js`（需 Node + Haxe 编译器）
- sha256 [embed] VSIX 整体: `104d785e3f7b57a7f3debf520d9751f7e7abf3a7e78d203db1a8ff3dc7ca30e2`；官方 sidecar（API 实测存在）: `.../file/nadako.vshaxe-{v}.sha256`

## 11. hlsl（shader-sense）

- pin `1.3.1`；latest `v1.5.0`（2026-09-07）
- URL: `https://github.com/antaalt/shader-sense/releases/download/v{v}/shader-language-server-{triple}.zip`；triple: W=`x86_64-pc-windows-msvc` **Wa=`aarch64-pc-windows-msvc`** L=`x86_64-unknown-linux-gnu`；**M/Ma 无预编译 → `cargo install shader_language_server --version {v} --locked`**
- bin: 根 `shader-language-server[.exe]`｜exec: `--stdio`
- sha256: pin [embed] W `49081c5547ddde1b8b3b17295282a80ddacbca1d6f5dcd834e2788c02baba997`｜Wa `a3b3799affe2cad27652e788376b46fe76e1a6c2ce45946a486dcb26c9091412`｜L `61710df7ca17a2d063b598936c57c56c49fbf837707a1aa886f9b0193a35be3c`；latest [API] W `571cdfdd61a0f144220ae5cce10cf80ca7da6d4cf7e1fb7d7203fb9f8cb71810`｜Wa `7a8101f6b15fbc1c424ba164274995f4a3b14e884d8a1e2112b5e3603a2f3beb`｜L `70ceadee70953924ff0957b141714c09061f31445d6ace666b781685c7d017ca`

## 12. kotlin（JetBrains CDN）

- pin `262.9593.0`；latest MISSING（CDN 无版本列表端点）
- URL（≥262.8190.0）: `https://download-cdn.jetbrains.com/language-server/kotlin-server/{v}/kotlin-server-{v}{suffix}`；suffix: W=`.win.zip` Wa=`-aarch64.win.zip` L=`.tar.gz` La=`-aarch64.tar.gz` M=`.sit`（zip 格式）Ma=`-aarch64.sit`
- archive: zip/tar.gz｜bin: `bin/intellij-server[.exe]`（linux 包内多一层 `kotlin-server-{v}/`）｜exec: `<launcher> --stdio --system-path <cache>/kotlin-lsp-system`，env `JAVA_TOOL_OPTIONS=-Xmx2G`
- legacy（<262.4739.0）: `/kotlin-lsp/{v}/kotlin-lsp-{v}-{win-x64|linux-x64|linux-aarch64|mac-x64|mac-aarch64}.zip`，launcher `kotlin-lsp.sh/.cmd`
- sha256 [hash-DB, pin 262.9593.0]: W `f2daaa476f26d99301b406f76de6d87c437d04dc72f06845154619d8f991c51f`｜Wa `73a552a6a420158622e5ad8d96b53da8aa8ced3f88a24fded01575927a2fd8e7`｜L `2d99d8e198fbe4aa8f4481e37799724ce94803b4ea12a60b416040e3fcd7cc5e`｜La `2317831c6e5607d05b7ebc1da655330125ce0e3d66fbf24517dfce442debc14e`｜M `17369fda97c85418ac24ab38a9df56b21522a3468dfe193832fe455c13920745`｜Ma `6ba6021a706b21e64cef33f7e2b79f187c0910320722bb2d3ed05ad1115ec43f`（JetBrains 无官方 digest 页）

## 13. lua（LuaLS）

- pin `3.15.0`；latest `3.19.1`（日期 MISSING）
- URL: `https://github.com/LuaLS/lua-language-server/releases/download/{v}/lua-language-server-{v}-{plat}`；plat: L=`linux-x64.tar.gz` La=`linux-arm64.tar.gz` M=`darwin-x64.tar.gz` Ma=`darwin-arm64.tar.gz` W=`win32-x64.zip`
- bin: `bin/lua-language-server[.exe]`｜exec: 无参
- sha256 [embed]: L `4877b874c52fb7587707898da9026cc3a6c854d9bbab115ef49ac4e6a1b88007`｜La `7dff8edfed4f34cf6325ff384791287d95f9a8dd9615a5279c7c6af81cf8c45d`｜M `01d28a31e264434e51662814a68f584af068393caecfa158c4df5f7fdc3ca2f7`｜Ma `050f5f493f65112afc116e31281a9f73918546782d3696485dc052724838f58b`｜W `76a10c05e8c947a448f00a61acead4240484cd1e2e8c66d54401c67d99b77535`

## 14. luau

- pin `1.63.0`；latest `1.69.0`（2026-07-18）
- URL: `https://github.com/JohnnyMorganz/luau-lsp/releases/download/{v}/luau-lsp-{plat}.zip`；plat: L=`linux-x86_64` La=`linux-arm64` M+Ma 共用 `macos`（universal）W=`win64`（无 Wa）
- bin: 根 `luau-lsp[.exe]`｜exec: `luau-lsp lsp [--definitions:@roblox=<globalTypes.*.d.luau>] [--docs=<en-us.json>]`；辅助文件 `https://luau-lsp.pages.dev/`（type-definitions/globalTypes.{level}.d.luau、api-docs/en-us.json、api-docs/luau-en-us.json）
- sha256: pin [embed] L `e4b633ad9a2c15437f60f9e721263f79aa0da606867d8458f0e159a325bf2db8`｜La `355be010f337a6772df6255c92e1fb28a59d194abe5c570453f4186472244355`｜M `01c1d6dd5fee27295b2968915dabb08c192192c46d9fe9c97bf31a130c96b8cb`｜W `eea596d47dc1c94a61ba1b78e6472bb4445bc3309780751515e6ab0a0abba57d`；latest [API] L `4457aeb690d3c22e04567f38c6259ac259a1673ec022758b9cb81af2a0e66c41`｜La `b0c78fe40defe71b9fa6381390a590f2040897980a0cf31f0a23c165ad27ebbb`｜M `4e93204901d892b227a4de15c9d4742176c15e9461d208cadafa1bcd58ec1ae3`｜W `faea1b177f4761e4c34e0dab72009a2fdd00f21d61f3f7b7c1fe1ac3f38a05d2`

## 15. matlab — VSIX 形态，需独立安装器设计

- pin `1.3.9`；latest MISSING
- URL 规律: `https://marketplace.visualstudio.com/_apis/public/gallery/publishers/MathWorks/vsextensions/language-matlab/{v}/vspackage`（universal VSIX=zip）
- bin: `extension/server/out/index.js`｜exec: `node index.js --stdio`，env `MATLAB_INSTALL_PATH`；需 MATLAB R2021b+ 与 Node
- sha256 [embed] VSIX 整体: `1da3add2c3a593fa0ebcdf1d15231faee8014de10f549c36915ab9d4f18390f2`（Marketplace 无官方 digest）

## 16. nextflow

- pin `26.04.3`；latest `v26.04.4`（2026-09-16）
- URL: `https://github.com/nextflow-io/language-server/releases/download/v{v}/language-server-all.jar`｜any（fat JAR, JDK≥17）
- archive: raw｜bin: JAR 本体｜exec: `java [jvm_opts] -jar <jar>`
- sha256: pin [hash-DB] `20cfa34f6e202d6b8babd8d786202ce00e0d39b70ccec3290e2ab3fbd02bc016`；latest [API] `e1c5da90c7f0565ba9e64ac3c5b58f1215e315d6bfe4414d62ff2d02749e8887`

## 17. omnisharp（Azure blob + visualstudio CDN）

- pin OmniSharp `1.39.10` + RazorOmnisharp `7.0.0-preview.23363.1`（JSON 11 个 OmniSharp 变体 + 9 个 Razor 变体，源自 dotnet/vscode-csharp，该上游已弃 OmniSharp 路线；serena 仅启用 L(net6)+W(net6)）
- URL 模板: `https://roslynomnisharp.blob.core.windows.net/releases/{v}/omnisharp-{plat}-{v}[-net6.0].zip`；Razor zip 为固定 GUID 路径（**不可模板化，只能整 URL 硬编码**，例 `https://download.visualstudio.microsoft.com/download/pr/aee63398-023f-48db-bba2-30162c68f0c4/6d4e23a3c7cf0465743950a39515a716/razorlanguageserver-linux-x64-7.0.0-preview.23363.1.zip`）
- archive: zip｜bin: `OmniSharp[.exe]`/`OmniSharp.dll`；Razor 插件 `OmniSharpPlugin/Microsoft.AspNetCore.Razor.OmniSharpPlugin.dll`｜exec: `<exe> -lsp --encoding ascii -z -s <sln> --hostPID <pid> … --plugin <razor.dll>`（.NET 6–9）
- sha256（integrity 字段=SHA256 大写 [embed]）: L(net6) `0926D3BEA060BF4373356B2FC0A68C10D0DE1B1150100B551BA5932814CE51E2`｜W(net6) `A73327395E7EF92C1D8E307055463DA412662C03F077ECC743462FD2760BB537`｜Razor L `4B08C47D70AA96BE40AEDE22CDFCD909EB9073F1E1992E2B5B99D9D7A47F3276`｜Razor W `8D2DC5484018A2390A2366D05CADDD2BE21F619122DACD1BA1A87815B98D7361`

## 18. pascal（pasls, zen010101 fork）

- pin = latest `v0.2.0`（2026-01-17）
- URL: `https://github.com/zen010101/pascal-language-server/releases/download/{v}/pasls-{plat}`；plat: L=`x86_64-linux.tar.gz` La=`aarch64-linux.tar.gz` M=`x86_64-darwin.zip` Ma=`aarch64-darwin.zip` W=`x86_64-win64.zip`（另有 `i386-win32.zip`）
- archive: unix=tar.gz win=zip｜bin: 根 `pasls`｜exec: 无参（env PP/FPCDIR/LAZARUSDIR/FPCTARGET*；需 FPC）
- sha256（embed = API = 官方 checksums.sha256 三方一致✓）: L `517259395b0a385a5e848cf48b967645a984be3dd456118bc08771283a822a5b`｜La `cb4986941cfdcf9cb74ece6bbb53a443390a908e880108a37f4ccf82b2d6c502`｜M `0abfcd98f63f77dba74094339a40d4407b69317c1c77b13a26b9b7dbdfd885f1`｜Ma `d4c2411e406af96ceae12b11e77fdb0c684ca15a68bfd8b4f9c6fe1fbdf515a7`｜W `1493c31552e6f90a59800b2d44669e01fc4551d3647f3cea5dd105a9f6bc73e5`；每 release 附 `checksums.sha256`

## 19. phpactor

- pin `2025.12.21.1`；latest `2026.06.23.0`（发布 2026-08-15）
- URL: `https://github.com/phpactor/phpactor/releases/download/{v}/phpactor.phar`｜any（PHAR, PHP 8.1+）
- archive: raw｜bin: `phpactor.phar`｜exec: `php phpactor.phar language-server`
- sha256: pin [embed] `53bbe9625cd9b5e9b394bc2f595fbad13dbbe6dfc96950c56dea3b5d9a246cc3`；latest [API] `25645647d9aa2dc69536fb4f75c976e33ef1a7b5533534a8456736e5e6fd5079`

## 20. phpantom（唯一全 6 平台）

- pin `0.8.0`；latest `0.10.0`（2026-08-20）
- URL: `https://github.com/PHPantom-dev/phpantom_lsp/releases/download/{v}/phpantom_lsp-{triple}`；triple: M=`x86_64-apple-darwin.tar.gz` Ma=`aarch64-apple-darwin.tar.gz` L=`x86_64-unknown-linux-gnu.tar.gz` La=`aarch64-unknown-linux-gnu.tar.gz` W=`x86_64-pc-windows-msvc.zip` Wa=`aarch64-pc-windows-msvc.zip`（另有 wasm32-wasip1）
- archive: unix=tar.gz win=zip｜bin: 根 `phpantom_lsp[.exe]`｜exec: `--stdio`（Rust 实现，零运行时依赖）
- sha256: pin [embed] W `17e23af816fc7ec695fe716f6209df6b0eafcec2fdafb5d9d72e2b352d5ddf83`｜Wa `352bfd90351c0f35947ea1af458dabe7a4dc4753d0cabaf9711a23a61346a63d`｜L `39615b495e624bbafe8787c3be61acabc123ec5ac23e9b30e00ab7660f50e020`｜La `e87fc96430f1bcc4966f953033a73a4e2ea53b2dbb7dc3e5f71cc8ced9022a57`｜M `e09eef93342cd38c9f9cc6c58064d81b005d06ec6d054e7cdeeec7698dc6c5da`｜Ma `2cdfd103b5df98d20712eaeea9bd00d2b459e2a588296f1ab8e558fe25fde456`；latest [API] W `b5473b8eed87a6cc4436af5662b89912d900c67832c49148913fa61298df92be`｜Wa `449efc300a51c5d21a632379031909d528a4ca1a3c2ea5d354b6f208008a3add`｜L `2b385588779cdfdb804371f99c9161b0ef41b6e57d34ed4e1b6963686f81dc20`｜La `89fa9d83dd8be9ac209ba30e42c972ac1979695cceccf81550cdc09453f35ac2`｜M `b1c8fffbbc34cba2edc42013f4010794179506eab04a49ee15a07364985bd596`｜Ma `2f445d9708ed15e1714b48271db43741d13a70c674ef5b3ff1a423e51b4f663b`

## 21. powershell（PSES）

- pin `4.4.0`；latest `v4.7.0`（2026-06-25）
- URL: `https://github.com/PowerShell/PowerShellEditorServices/releases/download/v{v}/PowerShellEditorServices.zip`｜any 平台单一 zip（另需 pwsh 7+、Save-Module PSScriptAnalyzer 1.25.0）
- archive: zip｜bin: `PowerShellEditorServices/Start-EditorServices.ps1`｜exec: `pwsh -NoLogo -NoProfile -Command "& '<script>' -HostName SolidLSP -HostProfileId solidlsp -HostVersion 1.0.0 -BundledModulesPath <dir> -LogPath <log> -LogLevel Information -SessionDetailsPath <json> -Stdio"`
- sha256: pin [embed] `690b91092989a0f66e6f43986166aaef69d64b559a9fda51feed882e1103fbcc`；latest [API] `5084f0326cc88539e9d880b08f6c52ce5be4672bb8ecee8921e35f8895a6b9d7`

## 22. systemverilog（verible）

- pin `v0.0-4051-g9fdb4057`；latest `v0.0-4283-ga1b0580b`（日期 MISSING）
- URL: `https://github.com/chipsalliance/verible/releases/download/{v}/verible-{v}-{plat}`；plat: L=`linux-static-x86_64.tar.gz` La=`linux-static-arm64.tar.gz` M+Ma 共用 `macOS.tar.gz`（dual-arch 同一资产同一 sha256）W=`win64.zip`（无 Wa）
- bin: `verible-{v}/bin/verible-verilog-ls[.exe]`｜exec: 无参
- sha256 [embed]: L `f52e5920ef63f70620a6086e09dea8bd778147cd7a9ff827bb7de5d6316b1754`｜La `30dd9c6f6e0f4840d6ba0c9e81ea2774a50b5a1a523a855245f9a9b4beb6b58b`｜M/Ma `9ef92e9ad345285dd593763e10ca61c8532fcf47bbb6cf4448f9a9423882d662`｜W `729aa244036da4a4f87bc026d33555456fc7f7be79778d983ebe9c893f4a0ca3`

## 23. toml（taplo）

- pin = latest `0.10.0`（2025-05-23）
- URL: `https://github.com/tamasfe/taplo/releases/download/{v}/taplo-{plat}`；plat: W=`windows-x86_64.zip`（另有 windows-x86.zip）M=`darwin-x86_64.gz` Ma=`darwin-aarch64.gz` L=`linux-x86_64.gz` La=`linux-aarch64.gz`（另有 linux-armv7/linux-riscv64/linux-x86）
- archive: win=zip 其余=**single-gz**｜bin: `taplo[.exe]`｜exec: `taplo lsp stdio`
- sha256 [embed]: W `1615eed140039bd58e7089109883b1c434de5d6de8f64a993e6e8c80ca57bdf9`｜win-x86 `b825701daab10dcfc0251e6d668cd1a9c0e351e7f6762dd20844c3f3f3553aa0`｜M `898122cde3a0b1cd1cbc2d52d3624f23338218c91b5ddb71518236a4c2c10ef2`｜Ma `713734314c3e71894b9e77513c5349835eefbd52908445a0d73b0c7dc469347d`｜L `8fe196b894ccf9072f0d4e1013a180306e17d244830b03986ee5e8eabeb6156`｜La `033681d01eec8376c3fd38fa3703c79316f5e14bb013d859943b60a07bccdcc3`｜armv7 `6b728896afe2573522f38b8e668b1ff40eb5928fd9d6d0c253ecae508274d417`。⚠️ 该 release GitHub API digest 字段全为 null → 真值仅 serena 内嵌 checksums

## 24. terraform（terraform-ls）

- pin `0.36.5`；latest 未查询（官方端点 `https://api.releases.hashicorp.com/v1/releases/terraform-ls/latest`）
- URL: `https://releases.hashicorp.com/terraform-ls/{v}/terraform-ls_{v}_{plat}.zip`；plat: M=`darwin_amd64` Ma=`darwin_arm64` L=`linux_amd64` La=`linux_arm64` W=`windows_amd64`（官方另有 windows_arm64，serena 未用）
- archive: zip｜bin: 根 `terraform-ls[.exe]`｜exec: 无参（需 terraform CLI 在 PATH）
- sha256（embed = 官方 SHA256SUMS 实调一致✓）: M `17c5c480f8eec7e528292565f1c05d5097a41edf7ef8ee2a9f3a18d288a1415a`｜Ma `fee8743aa71fe2d8b0b9b91283b844cfa57d58457306a62e53a8f38d143cec8c`｜L `37e645cc54fd03e863157e2a3e773e7a5ff1d6cb3d045e4c20860cac1f550a44`｜La `724f45029f32d02d88b1952c7d1526c59fc8cd5dae49e31b9fed676a83f6cae7`｜W `a9223462cac9e1c0e6ba33043fbf9fb4483609b6970b5681a6306b04366698ec`；官方校验 `{base}/terraform-ls_{v}_SHA256SUMS`

## 25. latex（texlab）

- pin `5.25.1`；latest `v5.26.0`（2026-06-25）
- URL: `https://github.com/latex-lsp/texlab/releases/download/v{v}/texlab-{plat}`；plat: M=`x86_64-macos.tar.gz` Ma=`aarch64-macos.tar.gz` L=`x86_64-linux.tar.gz` La=`aarch64-linux.tar.gz` W=`x86_64-windows.zip`（5.26.0 新增 aarch64-windows/i686-windows/armv7hf-linux/x86_64-alpine）
- archive: unix=tar.gz win=zip｜bin: 根 `texlab[.exe]`｜exec: 无参（需 LaTeX 工具链）
- sha256: pin [embed] M `11289a231f0cf382857a6a4a2eda1ba9f4f4e950af343b455797e3922d13b1ea`｜Ma `3755e9d1d4ad0b25135bdacd2fb453a612e88f48133185f96d660fa550398f66`｜L `c8260b2fd2849cbad7d1f54c4ffa0389f34664b049392107bc4f7f9c8ec542ba`｜La `e0d8e0b27b2e6e3526fa5019323bb3fddb1202a0f0049e527672b5ff323cc15e`｜W `aa5fc1fe6004c17cd83086a57a8c8f28bb3f360914872711bfbb83490dc3c19e`；latest [API] Ma `af7972ffd230711ba04ada9b69cc32ce9111d9196ba69538062872faefdbee56`｜La `a85cdfcd22454b8d8550f4b0f0620c45ab51760f302fac7a12bc18a890f70f8c`｜L `8697bd5e479d4584b14b7eed5c320c80ec4e1d91ebefbb6801e6bf38e9971300`｜aarch64-windows `99b215e9a44169eb8d786c33484b963055e1c3dd40f68e14fcc47ae1c84e92c1`；**latest M（x86_64-macos）与 W（x86_64-windows）digest MISSING**（API 响应截断，需复核）

---

## MISSING 汇总

1. al / matlab：latest 版本与日期（Marketplace 需 POST 查询；pin VSIX sha256 已有 embed 真值）
2. kotlin：latest 版本（CDN 无版本列表；pin 262.9593.0 全 6 平台 digest 已有）
3. terraform：latest 版本（官方端点已给，未实调）
4. 日期 MISSING：ada 2026.3.202607051、lua 3.19.1、verible v0.0-4283-ga1b0580b
5. latest digest MISSING：cue v0.17.1、ada 2026.3、lua 3.19.1、vscode-java v1.56、dart 3.13.4、taplo（GitHub digest=null）、clojure latest W、texlab latest M/W
6. VSIX/Marketplace 无官方 digest：al、matlab（haxe 例外，Open VSX 有 .sha256 sidecar）

## bump 复核工作流

```bash
gh api repos/<owner>/<repo>/releases/latest --jq '{tag:.tag_name, date:.published_at, assets:[.assets[]|{name,digest}]}'
# digest 形如 "sha256:<hex>"；旧 release 可能为 null（如 taplo 0.10.0）→ 下载后本地 sha256sum
```

## 交叉验证记录

- pascal v0.2.0：embed == GitHub API digest == 官方 checksums.sha256，5 平台全一致 ✓
- terraform 0.36.5：embed == 官方 SHA256SUMS，5 平台全一致 ✓
- gradle 8.14.2：hash-DB == services.gradle.org/.sha256 ✓
- haxe 2.34.2：pin == Open VSX latest 实测 ✓
- elixir expert：发现仓库迁移（elixir-lang → expert-lsp），serena 钉的 owner 已 302