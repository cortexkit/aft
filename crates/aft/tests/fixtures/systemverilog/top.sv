module top;
  import types_pkg::*;
  logic clk = 0;
  logic rst_n = 0;
  stream_if upstream(clk);
  stream_if downstream(clk);
  register_slice dut(clk, rst_n, upstream, downstream);

  always #5 clk = !clk;

  initial begin
    upstream.valid = 0;
    upstream.data = 0;
    downstream.ready = 1;
    repeat (2) @(negedge clk);
    rst_n = 1;
    upstream.data = increment(8'd41);
    upstream.valid = 1;
    do @(posedge clk); while (!upstream.ready);
    @(negedge clk);
    upstream.valid = 0;
    assert (downstream.valid && downstream.data == 8'd42) else $fatal(1, "bad buffer output");
    report(downstream.data);
    @(negedge clk);
    assert (!downstream.valid) else $fatal(1, "item was not consumed");
    $finish;
  end
endmodule
