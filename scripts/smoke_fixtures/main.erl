-module(main).

%% 冒烟具名符号：导出函数（documentSymbol 底线断言目标；erlang_ls 名含 /arity）
-export([smoke_double/1]).

smoke_double(X) -> X * 2.
