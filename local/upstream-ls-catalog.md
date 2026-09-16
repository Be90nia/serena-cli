# serena @43ae0211 — 73 个 LS 适配器安装机制全目录

来源: `oraios/serena` @ `43ae0211`, `src/solidlsp/language_servers/`。
调研方式: scout 子代理（read-only），主会话落地。

## 数量核对（重要）

- 顶层 74 个 `.py` = **73 个适配器文件 + `common.py`（共享代码，非适配器）**。
- **MSL 占 2 个文件算 1 个适配器**：`msl_language_server.py`（启动器）+ `msl_lsp_server.py`（随 serena 打包的 pygls 服务脚本，用 `sys.executable` 运行，无需安装）。
- Elixir 适配器在子目录 `elixir_tools/elixir_tools.py`。
- 因此 **73 个适配器 ↔ `ls_config.py` 中 `LanguageServerId` 枚举恰好 73 个值**（DENO 为最后一个），无缺失、无多出。

## 分类统计

| 分类 | 数量 | LS |
|---|---|---|
| A 单二进制下载 | **27** | ada, al, bsl, csharp, clangd, clojure, cue, dart, eclipse_jdtls, elixir, haxe, hlsl, kotlin, lua, luau, markdown(marksman), matlab, nextflow, omnisharp, pascal, phpactor, phpantom, powershell, systemverilog, toml(taplo), terraform, latex(texlab) |
| B npm 安装 | **14** | angular, ansible, bash, elm, php(intelephense), json, solidity, scss(some-sass), svelte, typescript, html, typescript_vts, vue, yaml |
| C pip（经 uvx） | **5** | python(pyright), python_basedpyright, python_pyrefly, python_ty, fortran(fortls) |
| D dotnet tool install | **1** | fsharp (fsautocomplete) |
| E gem install | **2** | ruby(ruby-lsp), ruby_solargraph |
| F 源码编译 | **1** | nix (nixd) |
| G 系统包/PATH（serena 只探测不安装） | **18** | cpp_ccls, crystal, deno, erlang, gleam, go(gopls), haskell, python_jedi, lean4, ocaml, perl, qml, r, rego(regal), rust, swift(sourcekit-lsp), wolfram, zig(zls) |
| H 其它 | **5** | gdscript(godot，连运行中的编辑器 TCP:6008), groovy(LS JAR 需用户自备，JRE 自动下载), julia(系统 julia + 用户自装 LanguageServer.jl), msl(serena 自带脚本), scala(coursier bootstrap) |

合计 27+14+5+1+2+1+18+5 = **73** ✓

## 主表（73 行全覆盖）

平台缩写：W=win-x64, Wa=win-arm64, L=linux-x64, La=linux-aarch64, M=darwin-x86_64, Ma=darwin-aarch64；"any"=平台无关资产。Runtime 列 = 运行 LS 所需外部运行时。

### A. 单二进制下载（27）

| # | LS (Language id) | 适配器文件 | 下载来源 / URL 模板 | 二进制平台 | Runtime |
|---|---|---|---|---|---|
| 1 | ada | ada_language_server.py | AdaCore releases: als-{v}-{plat}.tar.gz | L,La,M,Ma,W | 无（.gpr 项目更佳） |
| 2 | al | al_language_server.py | VS Code Marketplace VSIX | VSIX 多平台 | 无 |
| 3 | bsl | bsl_language_server.py | bsl-language-server-{v}-exec.jar | any (JAR) | **JVM ≥21** |
| 4 | csharp | csharp_language_server.py | NuGet 直下 roslyn-language-server.{plat}/{v} | W,Wa,M,Ma,L,La | **.NET 10+** |
| 6 | clojure | clojure_lsp.py | clojure-lsp-native-{plat}.zip | L,La,M,Ma,W | **Clojure CLI** |
| 7 | cue | cue_language_server.py | cue_v{v无v}_{plat}.tar.gz/.zip | L,La,M,Ma,W,Wa | 无 |
| 8 | dart | dart_language_server.py | dartsdk-{plat}-release.zip（整 SDK） | L,W,Wa,M,Ma | 无 |
| 9 | java (jdtls) | eclipse_jdtls.py | java-{plat}-{v}.vsix + gradle-{v}-bin.zip | L,La,M,Ma,W | **JDK ≥21** |
| 10 | elixir | elixir_tools/elixir_tools.py | expert_{plat}[.exe] | L,La,M,Ma,W | **Elixir** |
| 11 | haxe | haxe_language_server.py | Open VSX nadako.vshaxe-{v}.vsix | any (JS) | **Node.js** + Haxe 编译器 |
| 12 | hlsl | hlsl_language_server.py | shader-sense releases + mac 源 cargo install | W,Wa,L；M/Ma 源码 | 无 |
| 13 | kotlin | kotlin_language_server.py | JetBrains CDN kotlin-server-{v}{.win.zip\|-aarch64…} | W,Wa,L,La,M,Ma | JVM 新档案免 |
| 14 | lua | lua_ls.py | lua-language-server-{v}-{plat}.tar.gz/.zip | L,La,M,Ma,W | 无 |
| 15 | luau | luau_lsp.py | luau-lsp-{plat}.zip + luau-lsp.pages.dev 类型/文档 JSON | L,La,M(通用),W | 无 |
| 16 | markdown (marksman) | marksman.py | marksman-{plat} | L,La,M,Ma,W | 无 |
| 17 | matlab | matlab_language_server.py | VS Code Marketplace VSIX | VSIX 多平台 | **MATLAB R2021b+ + Node.js** |
| 18 | nextflow | nextflow_language_server.py | language-server-all.jar | any (fat JAR) | **JDK ≥17** |
| 19 | omnisharp | omnisharp.py + omnisharp/runtime_dependencies.json | roslynomnisharp releases + Razor 插件 | JSON 全 11 变体；启用 L,W | **.NET 6–9** |
| 20 | pascal | pascal_server.py | pasls-{plat}.tar.gz/.zip | L,La,M,Ma,W | FPC（完整功能需 PP/FPCDIR） |
| 21 | php_phpactor | phpactor.py | phpactor.phar | any (PHAR) | **PHP 8.1+** |
| 22 | php_phpantom | phpantom.py | phpantom_lsp-{plat}.tar.gz/.zip（**唯一全 6 平台**） | W,Wa,L,La,M,Ma | 无 |
| 23 | powershell | powershell_language_server.py | PowerShellEditorServices.zip | any (zip) | **PowerShell 7+ (pwsh)** |
| 24 | systemverilog | systemverilog_server.py | verible-{plat}.tar.gz/.zip | L,La,M+Ma,W（无 win-arm64） | 无 |
| 25 | toml (taplo) | taplo_server.py | taplo-{plat}.zip/.gz | W(x64/x86),M,Ma,L,La,armv7 | 无 |
| 26 | terraform | terraform_ls.py | releases.hashicorp.com terraform-ls | M,Ma,L,La,W | 无（terraform CLI 另需） |
| 27 | latex (texlab) | texlab_language_server.py | texlab-{plat}.tar.gz | M,Ma,L,La,W | LaTeX 工具链 |

### B. npm 安装（14，全部要求 Node.js + npm）

| # | LS | npm 包 | 默认版本 | 平台 |
|---|---|---|---|---|
| 28 | angular | `@angular/language-server` + `typescript`@5.9.3 + `typescript-language-server`@5.1.3 | 双进程 | any |
| 29 | ansible | `@ansible/ansible-language-server` | | any |
| 30 | bash | `bash-language-server` | | any |
| 31 | elm | `@elm-tooling/elm-language-server` + `elm` 编译器 | | any |
| 32 | php (intelephense) | `intelephense` | | any |
| 33 | json | `vscode-json-languageserver` | | any |
| 34 | solidity | `@nomicfoundation/solidity-language-server` + `@foundry-rs/forge-{plat}` | | any（forge 无 win-arm64） |
| 35 | scss | `some-sass-language-server` | | any |
| 36 | svelte | `svelte-language-server`@0.18.0 + typescript + typescript-language-server + `typescript-svelte-plugin`@0.3.52 | | any |
| 37 | typescript (默认) | `typescript`@5.9.3 + `typescript-language-server`@5.1.3 | | W,Wa,L,La,M,Ma |
| 38 | html | `vscode-langservers-extracted`@4.10.0 | | any |
| 39 | typescript_vts | `@vtsls/language-server`@0.2.9 | | any |
| 40 | vue | `@vue/language-server` + typescript + typescript-language-server | | any |
| 41 | yaml | `yaml-language-server` (Red Hat) | | any |

### C. pip / uvx（5）

`uvx -p 3.13 --from <pkg>==<v> <entrypoint>`，uv 自动拉 Python。

| # | LS | PyPI 包 | 入口 | 默认版本 |
|---|---|---|---|---|
| 42 | python (pyright, 默认) | `pyright` | `pyright-langserver --stdio` | 1.1.403 |
| 43 | python_basedpyright | `basedpyright` | `basedpyright-langserver --stdio` | (BASEDPYRIGHT_VERSION) |
| 44 | python_pyrefly | `pyrefly` | `pyrefly lsp` | (PYREFLY_VERSION) |
| 45 | python_ty | `ty` | `ty server` | (TY_VERSION) |
| 46 | fortran | `fortls` | `fortls` | (FORTLS_VERSION) |

### D. dotnet tool install（1）

| # | LS | 命令 | Runtime |
|---|---|---|---|
| 47 | fsharp | `dotnet tool install --tool-path ./ fsautocomplete --version {v}` | **.NET SDK** |

### E. gem install（2）

| # | LS | 安装 | Runtime |
|---|---|---|---|
| 48 | ruby (默认) | Bundler→Gemfile；否则 `gem install ruby-lsp -v {v}` | **Ruby** |
| 49 | ruby_solargraph | Bundler→`bundle exec solargraph`；否则 `gem install solargraph -v 0.51.1` | **Ruby** |

### F. 源码编译（1）

| # | LS | 安装 | Runtime |
|---|---|---|---|
| 50 | nix (nixd) | `nix profile install github:nix-community/nixd` 或 `nix-env -iA nixpkgs.nixd` | **Nix**（Windows 不支持） |

### G. 系统包 / PATH 探测，serena 不托管安装（18）

| # | LS | 需要的系统安装 |
|---|---|---|
| 51 | cpp_ccls | `ccls`：apt/dnf/pacman/zypper/emerge、brew、choco |
| 52 | crystal | `crystalline`（PATH） |
| 53 | deno | `deno` CLI（`deno lsp` 内置） |
| 54 | erlang | `erlang_ls`（PATH）+ Erlang runtime |
| 55 | gleam | `gleam`（`gleam lsp`） |
| 56 | go | **Go** + **gopls**（`go install golang.org/x/tools/gopls@…`） |
| 57 | haskell | `haskell-language-server-wrapper`（ghcup/stack/cabal/brew） |
| 58 | python_jedi | `jedi-language-server`（**无任何托管安装**，launch=`jedi-language-server`） |
| 59 | lean4 | `lean`（elan 安装） |
| 60 | ocaml | opam + `opam install ocaml-lsp-server` |
| 61 | perl | **Perl** + `cpanm Perl::LanguageServer` |
| 62 | qml | `qmlls`/`qmlls6`（Qt 6 随附） |
| 63 | r | **R** + CRAN `languageserver` |
| 64 | rego | `regal`（PATH） |
| 65 | rust | `rust-analyzer`（rustup component / cargo install） |
| 66 | swift | Xcode/Swift 工具链 |
| 67 | wolfram | **WolframKernel**（Mathematica 13+） |
| 68 | zig | zls + zig |

### H. 其它（5）

| # | LS | 机制 |
|---|---|---|
| 69 | gdscript (godot) | **零安装**：连接运行中的 Godot 编辑器内建 LSP（TCP 6008） |
| 70 | groovy | LS JAR **必须用户配置**（`ls_jar_path`）；JRE 自动下载 |
| 71 | julia | 系统 `julia` + 用户自装 `LanguageServer.jl` |
| 72 | msl | **serena 自带** pygls 服务脚本，`sys.executable msl_lsp_server.py` 启动 |
| 73 | scala | PATH 有 `metals` 则用之；否则 **coursier/cs bootstrap** `org.scalameta:metals_2.13:{v}` |

## Runtime 依赖标记（LS 启动所需外部运行时）

| Runtime | LS |
|---|---|
| **Node.js** | angular, ansible, bash, elm, haxe, intelephense, json, matlab, solidity, some-sass, svelte, typescript, vscode_html, vts, vue, yaml (16) |
| **JVM/Java** | bsl(≥21), eclipse_jdtls(JDK≥21), nextflow(JDK≥17), scala(JDK+coursier), groovy(JRE 自动), kotlin(自带 JBR) |
| **.NET/dotnet** | csharp(.NET 10+), fsharp(.NET SDK), omnisharp(.NET 6–9) |
| **Ruby** | ruby_lsp, solargraph |
| **Python** | msl(自带)；uvx 系 5 个由 uv 自动准备 Python 3.13 |
| **Go** | gopls |
| **PHP** | phpactor |
| **R** | r |
| **Julia** | julia |
| **Elixir** | elixir(expert) |
| **MATLAB** | matlab (+Node) |
| **Wolfram** | wolfram |
| **Swift/Xcode** | sourcekit |
| **Lean(elan)** | lean4 |
| **Deno CLI** | deno |
| **Zig** | zls |
| **Qt 6** | qml |
| **FPC** | pascal |
| **Clojure CLI** | clojure_lsp |
| **Nix** | nixd |
| **PowerShell 7+** | powershell |
| **Perl** | perl |
| **Godot 编辑器** | godot |
| **Rust 工具链（仅 hlsl/mac）** | hlsl |

## 找不全 / 需注意

- **无缺失**：73 个适配器全部定位并读取安装逻辑；无占位、无未实现。
- `common.py` 不是适配器；`omnisharp/*.json` 是 fixture 数据；`elixir_tools/README.md` 是文档。
- 分类边界：haxe（Open VSX）、al/matlab（VS Code Marketplace）、dart（Google storage）、kotlin（JetBrains CDN）、omnisharp（Azure blob）、terraform-ls（HashiCorp）、csharp（NuGet）均归「单二进制下载」——只有约 2/3 走 GitHub releases。
- jedi 是唯一「既不托管安装也不给安装指引」的适配器：直接 `jedi-language-server` 裸启动，PATH 无则失败。