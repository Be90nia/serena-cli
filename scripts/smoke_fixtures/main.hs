module Main where

-- 冒烟具名符号：类型签名 + 顶级函数（documentSymbol 底线断言目标）
smokeValue :: Int
smokeValue = 42

smokeDouble :: Int -> Int
smokeDouble x = x * 2
