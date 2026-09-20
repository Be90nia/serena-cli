# 上游 serena@43ae0211 — 71 个 LanguageServerId → 文件扩展名映射

> 锚：`oraios/serena@43ae0211`，`src/solidlsp/ls_config.py`（LanguageServerId 枚举 + get_source_fn_matcher）。
> 分类参照 `local/upstream-ls-catalog.md`（A=下载 / B=npm / C=uvx / D=dotnet / E=gem / F=源码 / G=PATH / H=其它）。
> 73 个 adapter 文件但 LanguageServerId 枚举 71 个值（差 2 为辅助文件如 `omnisharp/runtime_dependencies.json` + `msl_lsp_server.py`）。

## 主表

| LanguageServerId | 语言名 | 扩展名 | 分类 | 我们既有 |
|---|---|---|---|---|
| CSHARP | csharp | .cs | A(roslyn NuGet) | T2 |
| PYTHON | python | .py, .pyi | C(uvx pyright) | T2 |
| RUST | rust | .rs | G | T2 |
| JAVA | java | .java | A(jdtls) | T2 |
| KOTLIN | kotlin | .kt, .kts | A | 待加 |
| TYPESCRIPT | typescript | .ts, .tsx, .js, .jsx, .mts, .cts, .mjs, .cjs | B(npm ts) | 已存 |
| GO | go | .go | G(gopls) | T2 |
| RUBY | ruby | .rb, .erb | E(gem ruby-lsp) | 待加 |
| DART | dart | .dart | A(dart sdk) | 待加 |
| CPP | cpp | .c .h .c++ .cc .cp .cpp .cxx .hh .hpp .hxx .inl .ipp .tpp .txx .m .mm .c++m .cppm .cxxm .ixx .cu .hip .cl .clcpp .ino | A(clangd) | T2 |
| CPP_CCLS | cpp_ccls | .c .h .c++ .cc .cp .cpp .cxx .hh .hpp .hxx .inl .ipp .tpp .txx .m .mm .ino | G(ccls) | path_only ✓ |
| PHP | php | .php, .phtml | B(npm intelephense) | 待加 |
| R | r | .R, .r, .Rmd, .Rnw | G(R + languageserver) | path_only 否（exec 特殊）|
| PERL | perl | .pl, .pm, .t | G(Perl::LanguageServer) | path_only 否（启动特殊）|
| CLOJURE | clojure | .clj, .cljs, .cljc, .edn | A(clojure-lsp) | 待加 |
| ELIXIR | elixir | .ex, .exs | A(expert) | 待加 |
| ELM | elm | .elm | B(elm) | 待加 |
| TERRAFORM | terraform | .tf, .tfvars, .tfstate | A(tfls) | 待加 |
| SWIFT | swift | .swift | G(sourcekit-lsp) | path_only ✓ |
| BASH | bash | .sh, .bash | B(bash-language-server) | 待加 |
| CRYSTAL | crystal | .cr | G(crystalline) | path_only ✓ |
| CUE | cue | .cue | A(cue_ls) | 待加 |
| ZIG | zig | .zig, .zon | G(zls) | path_only ✓ |
| LUA | lua | .lua | A(lua-ls) | 待加 |
| LUAU | luau | .luau | A(luau-lsp) | 待加 |
| NIX | nix | .nix | F(nixd 源码) | 待加 |
| ERLANG | erlang | .erl, .hrl, .escript, .config, .app, .app.src | G(erlang_ls) | path_only ✓ |
| OCAML | ocaml | .ml, .mli, .re, .rei | G(ocamllsp) | path_only ✓ |
| AL | al | .al, .dal | A(VSIX) | 待加 |
| FSHARP | fsharp | .fs, .fsx, .fsi | D(dotnet fsautocomplete) | 待加 |
| REGO | rego | .rego | G(regal) | path_only ✓ |
| SCALA | scala | .scala, .sbt | H(metals coursier) | 待加 |
| JULIA | julia | .jl | H(用户配 LanguageServer.jl) | 待加 |
| FORTRAN | fortran | .f90, .f95, .f03, .f08, .f, .for, .fpp | C(uvx fortls) | 待加 |
| HASKELL | haskell | .hs, .lhs | G(haskell-language-server-wrapper) | path_only ✓ |
| HAXE | haxe | .hx | A(Open VSX vshaxe) | 待加 |
| LEAN4 | lean4 | .lean | G(elan lean --server) | path_only ✓ |
| GROOVY | groovy | .groovy, .gvy | H(用户配 ls_jar_path) | 待加 |
| VUE | vue | .vue, .ts/.tsx/.js/.jsx/.mts/.cts | B(@vue/language-server) | 待加 |
| SVELTE | svelte | .svelte, .ts/.js | B(svelte-language-server) | 待加 |
| POWERSHELL | powershell | .ps1, .psm1, .psd1 | A(PowerShellEditorServices) | 待加 |
| PASCAL | pascal | .pas, .pp, .lpr, .dpr, .dpk, .inc | A(pasls) | 待加 |
| MATLAB | matlab | .m, .mlx, .mlapp | A(VSIX matlab) | 待加 |
| MSL | msl | .mrc | H(serena 自带脚本) | 待加 |
| BSL | blob | .bsl, .os | A(JAR bsl-language-server) | 待加 |
| ADA | ada | .ads, .adb, .ada | A(als AdaCore) | 待加 |
| GDSCRIPT | gdscript | .gd, .gdscript | H(连 Godot 编辑器 TCP) | 待加 |
| QML | qml | .qml | G(qmlls) | path_only ✓ |
| GLEAM | gleam | .gleam | G(gleam lsp) | path_only ✓ |
| NEXTFLOW | nextflow | .nf | A(JAR language-server-all) | 待加 |
| WOLFRAM | wolfram | .wl, .wls | H(用户配 WolframKernel) | path_only 否 |
| TYPESCRIPT_VTS | typescript_vts | .ts, .tsx | B(@vtsls) | 待加 |
| PYTHON_JEDI | python_jedi | .py, .pyi | G(jedi-language-server) | path_only ✓ |
| PYTHON_TY | python_ty | .py, .pyi | C(uvx ty) | 待加 |
| PYTHON_PYREFLY | python_pyrefly | .py, .pyi | C(uvx pyrefly) | 待加 |
| PYTHON_BASEDPYRIGHT | python_basedpyright | .py, .pyi | C(uvx basedpyright) | 待加 |
| CSHARP_OMNISHARP | csharp_omnisharp | .cs | A(roslyn omnisharp) | 待加 |
| RUBY_SOLARGRAPH | ruby_solargraph | .rb | E(gem solargraph) | 待加 |
| PHP_PHPACTOR | php_phpactor | .php, .phtml | A(phpactor.phar) | 待加 |
| PHP_PHPANTOM | php_phpantom | .php, .phtml | A(phpantom_lsp) | 待加 |
| MARKDOWN | markdown | .md, .markdown | A(marksman) | download ✓ |
| LATEX | latex | .tex, .bib, .sty, .cls | A(texlab) | 待加 |
| YAML | yaml | .yaml, .yml | B(yaml-language-server) | 待加 |
| JSON | json | .json, .jsonc | B(vscode-json-languageserver) | 待加 |
| TOML | toml | .toml | A(taplo) | 待加 |
| HLSL | hlsl | .hlsl, .hlsli, .fx, .fxh, .cginc, .compute, .shader, .glsl, .vert, .frag, .geom, .tesc, .tese, .comp, .wgsl | A(shader-language-server) | 待加 |
| SYSTEMVERILOG | systemverilog | .sv, .svh, .v, .vh | A(verible) | 待加 |
| SOLIDITY | solidity | .sol | B(@nomicfoundation/solidity-language-server) | 待加 |
| ANSIBLE | ansible | .yaml, .yml | B(@ansible/ansible-language-server) | 待加 |
| HTML | html | .html, .htm | B(vscode-langservers-extracted) | 待加 |
| SCSS | scss | .scss, .sass, .css | B(some-sass-language-server) | 待加 |
| ANGULAR | angular | .html, .htm, .ts, .tsx | B(@angular/language-server) | 待加 |
| DENO | deno | .ts, .tsx, .js, .jsx, .mts, .cts, .mjs, .cjs | G(deno CLI lsp) | path_only ✓ |

## 我们的现状（按 catalog 分类）

- **T2 手写（7）**：cpp / rust / python / go / typescript / csharp / java
- **T0 download + path_only 已收录**（2）：markdown（marksman，A）/ crystalline（G）
- **T0 path_only 已收录（13 新）**：ccls / deno / erlang_ls / gleam / haskell_ls / jedi / lean4 / ocamllsp / qmlls / regal / sourcekit_lsp / zls（Task 20 f5ca3b9）
- **未收录 path_only（G 类未收）**：r / perl（exec 特殊待调研）
- **未收录（H 类）**：wolfram（用户配 WolframKernel）、scala（coursier bootstrap）、julia（用户装 LanguageServer.jl）、groovy（用户配 ls_jar_path）、gdscript（TCP）、msl（serena 自带）

## 扩展名映射要点（src/solidlsp/ls_config.py L413-542 get_source_fn_matcher）

- C/C++ 列了 ~28 个扩展（LLVM types lookup 表），含 CUDA/HIP/OpenCL/Arduino
- TypeScript/Deno/Vue/Angular/Svelte 都覆盖 .ts/.js 全系（c/m × x + base）
- R 大小写不敏感（.R/.r/.Rmd/.Rnw）
- Fortran 大小写不敏感（.f90/.F90/.f95...）
- Ada 大小写不敏感（.ads/.ADS）
- Angular 不覆盖 .scss（用 SCSS 适配器）—— 适配器分工明确

## 来源

- `LanguageServerId` 枚举值：ls_config.py L90-L317
- 扩展名映射：`get_source_fn_matcher` match 分支 L386-L637
- 适配器类名映射：`get_ls_class` L653-L870（用于按需 import 验证）
- 分类 / 安装机制：local/upstream-ls-catalog.md（已 scout 过 73 个适配器安装逻辑）

## 验收

- 71/71 行全覆盖 ✓（与枚举一一对应）
- 待加 = 53 条（71 - 18 已有/已列待）