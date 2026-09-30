unit Smoke;

interface

function Greet(const AName: string): string;

implementation

function Greet(const AName: string): string;
begin
  Result := 'Hello, ' + AName;
end;

end.
