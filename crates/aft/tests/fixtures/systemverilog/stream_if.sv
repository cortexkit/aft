interface stream_if(input logic clk);
  import types_pkg::*;
  data_t data;
  logic valid;
  logic ready;

  modport source(output data, valid, input ready, clk);
  modport sink(input data, valid, output ready, input clk);
endinterface
