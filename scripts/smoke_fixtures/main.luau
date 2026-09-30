-- luau smoke fixture: typed Luau with one exportable symbol
local Smoke = {}

function Smoke.greet(name: string): string
	return `Hello, {name}!`
end

export type Config = {
	debug: boolean,
}

return Smoke
