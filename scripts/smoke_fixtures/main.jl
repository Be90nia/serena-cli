struct SmokePoint
    x::Float64
    y::Float64
end

function smoke_distance(a::SmokePoint, b::SmokePoint)
    sqrt((a.x - b.x)^2 + (a.y - b.y)^2)
end
