package sample_pkg;
  function automatic int increment(input int value);
    return value + 1;
  endfunction
endpackage

interface sample_if;
  logic valid;
endinterface

module sample;
  import sample_pkg::*;
  sample_if bus();
  task automatic report(input int value);
    $display("value=%0d", value);
  endtask
  initial report(increment(41));
endmodule
