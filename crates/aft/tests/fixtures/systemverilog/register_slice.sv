// One-entry ready/valid buffer. Accepts a new item when empty or consumed.
module register_slice(
  input logic clk,
  input logic rst_n,
  stream_if.sink upstream,
  stream_if.source downstream
);
  import types_pkg::*;
  data_t stored;
  logic full;

  assign upstream.ready = !full || downstream.ready;
  assign downstream.valid = full;
  assign downstream.data = stored;

  always_ff @(posedge clk) begin
    if (!rst_n) begin
      full <= 1'b0;
    end else if (upstream.ready) begin
      full <= upstream.valid;
      if (upstream.valid) stored <= upstream.data;
    end
  end
endmodule
