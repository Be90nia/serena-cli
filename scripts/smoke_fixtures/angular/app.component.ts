import { Component } from '@angular/core';

@Component({
  selector: 'app-smoke',
  template: '<p>smoke works</p>',
})
export class SmokeComponent {
  title = 'smoke';

  boost(factor: number): number {
    return factor * 2;
  }
}
