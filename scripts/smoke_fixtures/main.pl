#!/usr/bin/perl
use strict;
use warnings;

# 冒烟具名符号：具名子例程（documentSymbol 底线断言目标）
sub smoke_double {
    my ($x) = @_;
    return $x * 2;
}

1;
