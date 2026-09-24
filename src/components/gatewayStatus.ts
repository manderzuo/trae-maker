/** A click based on an out-of-date badge must not perform the opposite action. */
export function shouldIgnoreStaleGatewayToggle(displayedRunning: boolean, actualRunning: boolean): boolean {
  return displayedRunning !== actualRunning;
}
