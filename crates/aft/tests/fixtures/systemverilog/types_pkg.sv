`include "config.svh"
package types_pkg;
  typedef logic [`STREAM_WIDTH-1:0] data_t;

  function automatic data_t increment(input data_t value);
    return value + 1'b1;
  endfunction

  task automatic report(input data_t value);
    $display("value=%0d", value);
  endtask
endpackage
