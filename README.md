# sdkmode

A sandboxed agent that writes JavaScript instead of tool-calls and brokers credentials for external requests. 

## Installation

### Linux/Mac ・Recommended

```sh 
curl -fsSL https://sh.sdkmo.de | sh
```

### Mac ・Homebrew

<!-- TODO: homebrew -->
Coming soon. 

### Windows

<!-- TODO: windows -->
Coming soon. 

## Usage

```sh
sdkmode
```

## Benchmark 

<table>
<thead>
<tr>
<th>Task</th><th>Engine</th><th align="right">Correct</th><th align="right">Cost (mean&nbsp;±&nbsp;sd)</th><th align="right">Time</th><th align="right">sdkmode</th>
</tr>
</thead>
<tbody>

<tr>
<td rowspan="2"><code>open-issues</code></td>
<td>sdkmode</td><td align="right">9/10</td><td align="right"><strong>$0.014</strong> ±.009</td><td align="right">13.8s</td>
<td rowspan="2" align="right"><strong>6× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">10/10</td><td align="right">$0.081 ±.014</td><td align="right">29.7s</td></tr>

<tr>
<td rowspan="2"><code>default-branch</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.005</strong> ±.000</td><td align="right">4.9s</td>
<td rowspan="2" align="right"><strong>11× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">10/10</td><td align="right">$0.055 ±.021</td><td align="right">10.4s</td></tr>

<tr>
<td rowspan="2"><code>latest-release</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.005</strong> ±.000</td><td align="right">5.2s</td>
<td rowspan="2" align="right"><strong>13× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">10/10</td><td align="right">$0.063 ±.025</td><td align="right">10.1s</td></tr>

<tr>
<td rowspan="2"><code>license</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.005</strong> ±.000</td><td align="right">6.0s</td>
<td rowspan="2" align="right"><strong>9× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">10/10</td><td align="right">$0.047 ±.021</td><td align="right">8.6s</td></tr>

<tr>
<td rowspan="2"><code>total-stars</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.007</strong> ±.000</td><td align="right">9.0s</td>
<td rowspan="2" align="right"><strong>10× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">9/10</td><td align="right">$0.072 ±.023</td><td align="right">12.8s</td></tr>

<tr>
<td rowspan="2"><code>most-issues-repo</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.008</strong> ±.000</td><td align="right">8.5s</td>
<td rowspan="2" align="right"><strong>11× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">9/10</td><td align="right">$0.085 ±.026</td><td align="right">28.0s</td></tr>

<tr>
<td rowspan="2"><code>recent-with-issues</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.012</strong> ±.003</td><td align="right">12.8s</td>
<td rowspan="2" align="right"><strong>9× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">10/10</td><td align="right">$0.111 ±.020</td><td align="right">35.3s</td></tr>

<tr>
<td rowspan="2"><code>top-starred-repo</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.008</strong> ±.001</td><td align="right">12.9s</td>
<td rowspan="2" align="right"><strong>8× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">10/10</td><td align="right">$0.064 ±.018</td><td align="right">18.8s</td></tr>

<tr>
<td rowspan="2"><code>newest-repo</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.006</strong> ±.000</td><td align="right">5.8s</td>
<td rowspan="2" align="right"><strong>11× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">7/10</td><td align="right">$0.067 ±.015</td><td align="right">13.2s</td></tr>

<tr>
<td rowspan="2"><code>starred-top-100</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.013</strong> ±.003</td><td align="right">19.5s</td>
<td rowspan="2" align="right"><strong>7× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">5/10</td><td align="right">$0.087 ±.014</td><td align="right">28.2s</td></tr>

<tr>
<td rowspan="2"><code>issue-starred-100</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.019</strong> ±.007</td><td align="right">24.9s</td>
<td rowspan="2" align="right"><strong>8× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">9/10</td><td align="right">$0.154 ±.031</td><td align="right">52.7s</td></tr>

<tr>
<td rowspan="2"><code>rust-files</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.020</strong> ±.042</td><td align="right">6.5s</td>
<td rowspan="2" align="right">1.5× cheaper</td>
</tr>
<tr><td>claude-code</td><td align="right">10/10</td><td align="right">$0.029 ±.026</td><td align="right">8.5s</td></tr>

<tr>
<td rowspan="2"><code>crate-version</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.014</strong> ±.023</td><td align="right">7.4s</td>
<td rowspan="2" align="right">2.4× cheaper</td>
</tr>
<tr><td>claude-code</td><td align="right">10/10</td><td align="right">$0.033 ±.033</td><td align="right">9.4s</td></tr>

<tr>
<td rowspan="2"><code>local-add-fn</code></td>
<td>sdkmode</td><td align="right">10/10</td><td align="right"><strong>$0.013</strong> ±.003</td><td align="right">11.3s</td>
<td rowspan="2" align="right"><strong>8× cheaper</strong></td>
</tr>
<tr><td>claude-code</td><td align="right">10/10</td><td align="right">$0.101 ±.001</td><td align="right">14.5s</td></tr>

</tbody>
</table>
