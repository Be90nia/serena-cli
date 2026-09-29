extends Node

var smoke_speed: float = 1.0

func smoke_tick(delta: float) -> void:
	smoke_speed += delta
