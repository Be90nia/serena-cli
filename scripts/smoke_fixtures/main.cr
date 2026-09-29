# 冒烟具名符号：具名方法/常量（documentSymbol 底线断言目标）
SMOKE_VALUE = 42

def smoke_double(x)
  x * 2
end

puts smoke_double(SMOKE_VALUE)
