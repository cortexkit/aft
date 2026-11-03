
unit MyUnit;

interface

uses SysUtils;

type
  TMyClass = class
  private
    FValue: Integer;
  public
    constructor Create;
    procedure DoSomething; virtual;
  end;

  TMyRecord = record
    X, Y: Integer;
  end;

  IMyInterface = interface
    procedure DoSomethingElse;
  end;

  TMyEnum = (Red, Green, Blue);

const
  UNIT_CONST = 42;

var
  UnitVar: string;

implementation

constructor TMyClass.Create;
begin
  FValue := 0;
end;

procedure TMyClass.DoSomething;
begin
end;

procedure StandaloneProc;
begin
end;

end.
