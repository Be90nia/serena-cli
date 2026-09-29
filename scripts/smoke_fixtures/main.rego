package smoke

default allow = false

allow if input.role == "admin"

deny contains msg if {
    input.role != "admin"
    msg := "forbidden"
}
