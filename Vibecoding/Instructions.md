Can you create a test application in rust that uses the box3d library?

Put it in a seperate folder called RustTest
It should show a 3d landscape with a house and a ragdoll



Commit and push
Add a version nr to the 3d modeler (show in footer)
v 0.1.0
Increment last digit of the version nr every time you build and commit


Overall features

- lighting

Implement lights, and lighting modes.
Lights can have different colors and intensities.
It should cast shadows, reflect off surfaces, and affect the overall lighting of the scene.

- shadows
- reflections
- rendering

- smoke and particle effects

- glass and transparent materials


UI improvements
- make the menu items fill the width of the toolbar
- use color accents for better visual hierarchy
- add color themes (include a light / dark theme)


Add the color theme selection in the view menu

Add an option to the wall object to break it down in individual blocks/bricks. All with physics.

Add the possibility to add folders to outliner. Objects can then be put in these folders. 
Put the individual bricks in a folder when transforming a wall into bricks.

Physics mode
When pressing space, the app is in physics mode.
When this mode is active, the mouse left mouse click release applies a force to the objects.
The longer the mouse is held down, the stronger the force.


In the add object menu, or shift-A menu, add a small icon, pictogram of the object.


Add an empty point as a new object. (3 lines, x,y,z)
